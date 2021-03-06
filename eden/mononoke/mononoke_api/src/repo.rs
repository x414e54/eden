/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::fmt;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use aclchecker::AclChecker;
use anyhow::{bail, format_err, Error};
use blobrepo::BlobRepo;
use blobrepo_factory::{BlobrepoBuilder, BlobstoreOptions, Caching, ReadOnlyStorage};
use blobstore::Loadable;
use blobstore_factory::make_sql_factory;
use bookmarks::{BookmarkName, BookmarkPrefix};
use changeset_info::ChangesetInfo;
use context::CoreContext;
use cross_repo_sync::{CommitSyncRepos, CommitSyncer};
use derived_data::BonsaiDerived;
use fbinit::FacebookInit;
use filestore::{Alias, FetchKey};
use futures::compat::{Future01CompatExt, Stream01CompatExt};
use futures::future::{self, try_join, try_join_all, TryFutureExt};
use futures::StreamExt as NewStreamExt;
use futures_ext::StreamExt;
use futures_old::stream::{self, Stream};
use identity::Identity;
use itertools::Itertools;
use mercurial_types::Globalrev;
use metaconfig_types::{
    CommitSyncConfig, CommonConfig, RepoConfig, SourceControlServiceMonitoring,
    SourceControlServiceParams,
};
use mononoke_types::{
    hash::{GitSha1, Sha1, Sha256},
    Generation,
};
use revset::AncestorsNodeStream;
use skiplist::{fetch_skiplist_index, SkiplistIndex};
use slog::{debug, error, Logger};
use sql_ext::facebook::MysqlOptions;
#[cfg(test)]
use sql_ext::SqlConstructors;
use stats_facebook::service_data::{get_service_data_singleton, ServiceData};
use std::collections::HashSet;
use synced_commit_mapping::{SqlSyncedCommitMapping, SyncedCommitMapping};
use warm_bookmarks_cache::WarmBookmarksCache;

use crate::changeset::ChangesetContext;
use crate::errors::MononokeError;
use crate::file::{FileContext, FileId};
use crate::hg::HgRepoContext;
use crate::repo_write::RepoWriteContext;
use crate::specifiers::{
    ChangesetId, ChangesetPrefixSpecifier, ChangesetSpecifier, ChangesetSpecifierPrefixResolution,
    HgChangesetId,
};
use crate::tree::{TreeContext, TreeId};

const COMMON_COUNTER_PREFIX: &'static str = "mononoke.api";
const STALENESS_INFIX: &'static str = "staleness.secs";
const MISSING_FROM_CACHE_INFIX: &'static str = "missing_from_cache";
const MISSING_FROM_REPO_INFIX: &'static str = "missing_from_repo";
const ACL_CHECKER_TIMEOUT_MS: u32 = 10_000;

pub(crate) struct Repo {
    pub(crate) name: String,
    pub(crate) blob_repo: BlobRepo,
    pub(crate) skiplist_index: Arc<SkiplistIndex>,
    pub(crate) warm_bookmarks_cache: Arc<WarmBookmarksCache>,
    // This doesn't really belong here, but until we have production mappings, we can't do a better job
    pub(crate) synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    pub(crate) service_config: SourceControlServiceParams,
    // Needed to report stats
    pub(crate) monitoring_config: Option<SourceControlServiceMonitoring>,
    pub(crate) acl_checker: Option<Arc<AclChecker>>,
    pub(crate) commit_sync_config: Option<CommitSyncConfig>,
}

#[derive(Clone)]
pub struct RepoContext {
    ctx: CoreContext,
    repo: Arc<Repo>,
}

impl fmt::Debug for RepoContext {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "RepoContext(repo={:?})", self.name())
    }
}

pub async fn open_synced_commit_mapping(
    fb: FacebookInit,
    config: RepoConfig,
    mysql_options: MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    logger: &Logger,
) -> Result<Arc<SqlSyncedCommitMapping>, Error> {
    let sql_factory = make_sql_factory(
        fb,
        config.storage_config.dbconfig,
        mysql_options,
        readonly_storage,
        logger.clone(),
    )
    .compat()
    .await?;

    sql_factory.open::<SqlSyncedCommitMapping>().compat().await
}

impl Repo {
    pub(crate) async fn new(
        fb: FacebookInit,
        logger: Logger,
        name: String,
        config: RepoConfig,
        common_config: CommonConfig,
        mysql_options: MysqlOptions,
        with_cachelib: Caching,
        readonly_storage: ReadOnlyStorage,
        blobstore_options: BlobstoreOptions,
    ) -> Result<Self, Error> {
        let skiplist_index_blobstore_key = config.skiplist_index_blobstore_key.clone();

        let synced_commit_mapping = open_synced_commit_mapping(
            fb,
            config.clone(),
            mysql_options,
            readonly_storage,
            &logger,
        )
        .await?;
        let service_config = config.source_control_service.clone();
        let monitoring_config = config.source_control_service_monitoring.clone();

        let builder = BlobrepoBuilder::new(
            fb,
            name.clone(),
            &config,
            mysql_options,
            with_cachelib,
            common_config.scuba_censored_table,
            readonly_storage,
            blobstore_options,
            &logger,
        );
        let blob_repo = builder.build().await?;

        let ctx = CoreContext::new_with_logger(fb, logger.clone());

        let acl_checker = tokio::task::spawn_blocking({
            let acl = config.hipster_acl;
            move || match &acl {
                Some(acl) => {
                    let id = Identity::new("REPO", &acl);
                    let acl_checker = Arc::new(AclChecker::new(fb, &id)?);
                    if acl_checker.do_wait_updated(ACL_CHECKER_TIMEOUT_MS) {
                        Ok(Some(acl_checker))
                    } else {
                        bail!("Failed to update AclChecker")
                    }
                }
                None => Ok(None),
            }
        })
        .map_err(|e| anyhow::Error::new(e))
        .and_then(|r| future::ready(r));

        let skiplist_index = fetch_skiplist_index(
            ctx.clone(),
            skiplist_index_blobstore_key,
            blob_repo.get_blobstore().boxed(),
        )
        .compat();

        let warm_bookmarks_cache = Arc::new(
            WarmBookmarksCache::new(ctx.clone(), blob_repo.clone())
                .compat()
                .await?,
        );

        let (acl_checker, skiplist_index) = try_join(acl_checker, skiplist_index).await?;

        Ok(Self {
            name,
            blob_repo,
            skiplist_index,
            warm_bookmarks_cache,
            synced_commit_mapping,
            service_config,
            monitoring_config,
            acl_checker,
            commit_sync_config: config.commit_sync_config,
        })
    }

    /// Temporary function to create directly from parts.
    pub(crate) fn new_from_parts(
        name: String,
        blob_repo: BlobRepo,
        skiplist_index: Arc<SkiplistIndex>,
        warm_bookmarks_cache: Arc<WarmBookmarksCache>,
        synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
        monitoring_config: Option<SourceControlServiceMonitoring>,
        commit_sync_config: Option<CommitSyncConfig>,
    ) -> Self {
        Self {
            name,
            blob_repo,
            skiplist_index,
            warm_bookmarks_cache,
            synced_commit_mapping,
            service_config: SourceControlServiceParams {
                permit_writes: false,
            },
            monitoring_config,
            acl_checker: None,
            commit_sync_config,
        }
    }

    #[cfg(test)]
    /// Construct a Repo from a test BlobRepo
    pub(crate) async fn new_test(ctx: CoreContext, blob_repo: BlobRepo) -> Result<Self, Error> {
        Self::new_test_common(
            ctx,
            blob_repo,
            None,
            Arc::new(SqlSyncedCommitMapping::with_sqlite_in_memory()?),
        )
        .await
    }

    #[cfg(test)]
    /// Construct a Repo from a test BlobRepo and commit_sync_config
    pub(crate) async fn new_test_xrepo(
        ctx: CoreContext,
        blob_repo: BlobRepo,
        commit_sync_config: CommitSyncConfig,
        synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    ) -> Result<Self, Error> {
        Self::new_test_common(
            ctx,
            blob_repo,
            Some(commit_sync_config),
            synced_commit_mapping,
        )
        .await
    }

    #[cfg(test)]
    /// Construct a Repo from a test BlobRepo and commit_sync_config
    async fn new_test_common(
        ctx: CoreContext,
        blob_repo: BlobRepo,
        commit_sync_config: Option<CommitSyncConfig>,
        synced_commit_mapping: Arc<dyn SyncedCommitMapping>,
    ) -> Result<Self, Error> {
        let warm_bookmarks_cache = Arc::new(
            WarmBookmarksCache::new(ctx.clone(), blob_repo.clone())
                .compat()
                .await?,
        );
        Ok(Self {
            name: String::from("test"),
            blob_repo,
            skiplist_index: Arc::new(SkiplistIndex::new()),
            warm_bookmarks_cache,
            synced_commit_mapping,
            service_config: SourceControlServiceParams {
                permit_writes: true,
            },
            monitoring_config: None,
            acl_checker: None,
            commit_sync_config,
        })
    }

    pub async fn report_monitoring_stats(&self, ctx: &CoreContext) -> Result<(), MononokeError> {
        match self.monitoring_config.as_ref() {
            None => Ok(()),
            Some(monitoring_config) => {
                let reporting_futs = monitoring_config
                    .bookmarks_to_report_age
                    .iter()
                    .map(move |bookmark| self.report_bookmark_age_difference(ctx, &bookmark));
                try_join_all(reporting_futs).await.map(|_| ())
            }
        }
    }

    fn set_counter(&self, ctx: &CoreContext, name: &dyn AsRef<str>, value: i64) {
        get_service_data_singleton(ctx.fb).set_counter(name, value);
    }

    fn report_bookmark_missing_from_cache(&self, ctx: &CoreContext, bookmark: &BookmarkName) {
        error!(
            ctx.logger(),
            "Monitored bookmark does not exist in the cache: {}", bookmark
        );

        let counter_name = format!(
            "{}.{}.{}.{}",
            COMMON_COUNTER_PREFIX,
            MISSING_FROM_CACHE_INFIX,
            self.blob_repo.get_repoid(),
            bookmark,
        );
        self.set_counter(ctx, &counter_name, 1);
    }

    fn report_bookmark_missing_from_repo(&self, ctx: &CoreContext, bookmark: &BookmarkName) {
        error!(
            ctx.logger(),
            "Monitored bookmark does not exist in the repo: {}", bookmark
        );

        let counter_name = format!(
            "{}.{}.{}.{}",
            COMMON_COUNTER_PREFIX,
            MISSING_FROM_REPO_INFIX,
            self.blob_repo.get_repoid(),
            bookmark,
        );
        self.set_counter(ctx, &counter_name, 1);
    }

    fn report_bookmark_staleness(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkName,
        staleness: i64,
    ) {
        debug!(
            ctx.logger(),
            "Reporting staleness of {} in repo {} to be {}s",
            bookmark,
            self.blob_repo.get_repoid(),
            staleness
        );

        let counter_name = format!(
            "{}.{}.{}.{}",
            COMMON_COUNTER_PREFIX,
            STALENESS_INFIX,
            self.blob_repo.get_repoid(),
            bookmark,
        );
        self.set_counter(ctx, &counter_name, staleness);
    }

    async fn report_bookmark_age_difference(
        &self,
        ctx: &CoreContext,
        bookmark: &BookmarkName,
    ) -> Result<(), MononokeError> {
        let repo = &self.blob_repo;

        let maybe_bcs_id_from_service = self.warm_bookmarks_cache.get(bookmark);
        let maybe_bcs_id_from_blobrepo = repo
            .get_bonsai_bookmark(ctx.clone(), &bookmark)
            .compat()
            .await?;

        if maybe_bcs_id_from_blobrepo.is_none() {
            self.report_bookmark_missing_from_repo(ctx, bookmark);
        }

        if maybe_bcs_id_from_service.is_none() {
            self.report_bookmark_missing_from_cache(ctx, bookmark);
        }

        if let (Some(service_bcs_id), Some(blobrepo_bcs_id)) =
            (maybe_bcs_id_from_service, maybe_bcs_id_from_blobrepo)
        {
            // We report the difference between current time (i.e. SystemTime::now())
            // and timestamp of the first child of bookmark value from cache (see graph below)
            //
            //       O <- bookmark value from blobrepo
            //       |
            //      ...
            //       |
            //       O <- first child of bookmark value from cache.
            //       |
            //       O <- bookmark value from cache, it's outdated
            //
            // This way of reporting shows for how long the oldest commit not in cache hasn't been
            // imported, and it should work correctly both for high and low commit rates.
            debug!(
                ctx.logger(),
                "Reporting bookmark age difference for {}: latest {} value is {}, cache points to {}",
                repo.get_repoid(),
                bookmark,
                blobrepo_bcs_id,
                service_bcs_id,
            );

            let difference = if blobrepo_bcs_id == service_bcs_id {
                0
            } else {
                let limit = 100;
                let maybe_child = self
                    .try_find_child(ctx, service_bcs_id, blobrepo_bcs_id, limit)
                    .await?;

                // If we can't find a child of a bookmark value from cache, then it might mean
                // that either cache is too far behind or there was a non-forward bookmark move.
                // Either way, we can't really do much about it here, so let's just find difference
                // between current timestamp and bookmark value from cache.
                let compare_bcs_id = maybe_child.unwrap_or(service_bcs_id);

                let compare_timestamp = compare_bcs_id
                    .load(ctx.clone(), repo.blobstore())
                    .compat()
                    .await?
                    .author_date()
                    .timestamp_secs();

                let current_timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(Error::from)?;
                let current_timestamp = current_timestamp.as_secs() as i64;
                current_timestamp - compare_timestamp
            };
            self.report_bookmark_staleness(ctx, bookmark, difference);
        }

        Ok(())
    }

    /// Try to find a changeset that's ancestor of `descendant` and direct child of
    /// `ancestor`. Returns None if this commit doesn't exist (for example if `ancestor` is not
    /// actually an ancestor of `descendant`) or if child is too far away from descendant.
    async fn try_find_child(
        &self,
        ctx: &CoreContext,
        ancestor: ChangesetId,
        descendant: ChangesetId,
        limit: u64,
    ) -> Result<Option<ChangesetId>, Error> {
        // This is a generation number beyond which we don't need to traverse
        let min_gen_num = self.fetch_gen_num(ctx, &ancestor).await?;

        let mut ancestors = AncestorsNodeStream::new(
            ctx.clone(),
            &self.blob_repo.get_changeset_fetcher(),
            descendant,
        )
        .compat();

        let mut traversed = 0;
        while let Some(cs_id) = ancestors.next().await {
            traversed += 1;
            if traversed > limit {
                return Ok(None);
            }

            let cs_id = cs_id?;
            let parents = self
                .blob_repo
                .get_changeset_parents_by_bonsai(ctx.clone(), cs_id)
                .compat()
                .await?;

            if parents.contains(&ancestor) {
                return Ok(Some(cs_id));
            } else {
                let gen_num = self.fetch_gen_num(ctx, &cs_id).await?;
                if gen_num < min_gen_num {
                    return Ok(None);
                }
            }
        }

        Ok(None)
    }

    async fn fetch_gen_num(
        &self,
        ctx: &CoreContext,
        cs_id: &ChangesetId,
    ) -> Result<Generation, Error> {
        let maybe_gen_num = self
            .blob_repo
            .get_generation_number(ctx.clone(), *cs_id)
            .compat()
            .await?;
        maybe_gen_num.ok_or(format_err!("gen num for {} not found", cs_id))
    }

    fn check_acl(&self, ctx: &CoreContext, mode: &'static str) -> Result<(), MononokeError> {
        if let Some(acl_checker) = self.acl_checker.as_ref() {
            let identities = ctx.identities();
            let permitted = identities
                .as_ref()
                .map(|identities| acl_checker.check_set(&identities, &[mode]))
                .unwrap_or(false);
            if !permitted {
                debug!(
                    ctx.logger(),
                    "Permission denied: {} access to {}", mode, self.name
                );
                let identities = identities
                    .as_ref()
                    .map(|identities| identities.to_string())
                    .unwrap_or_else(|| "<none>".to_string());
                return Err(MononokeError::PermissionDenied {
                    mode,
                    identities,
                    reponame: self.name.clone(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Stack {
    pub draft: HashSet<ChangesetId>,
    pub public: HashSet<ChangesetId>,
}

/// A context object representing a query to a particular repo.
impl RepoContext {
    pub(crate) fn new(ctx: CoreContext, repo: Arc<Repo>) -> Result<Self, MononokeError> {
        // Check the user is permitted to access this repo.
        repo.check_acl(&ctx, "read")?;
        Ok(Self { repo, ctx })
    }

    /// The context for this query.
    pub(crate) fn ctx(&self) -> &CoreContext {
        &self.ctx
    }

    /// The name of the underlying repo.
    pub(crate) fn name(&self) -> &str {
        &self.repo.name
    }

    /// The underlying `BlobRepo`.
    pub(crate) fn blob_repo(&self) -> &BlobRepo {
        &self.repo.blob_repo
    }

    /// The skiplist index for the referenced repository.
    pub(crate) fn skiplist_index(&self) -> &SkiplistIndex {
        &self.repo.skiplist_index
    }

    /// The commit sync mapping for the referenced repository
    pub(crate) fn synced_commit_mapping(&self) -> &Arc<dyn SyncedCommitMapping> {
        &self.repo.synced_commit_mapping
    }

    /// The warm bookmarks cache for the referenced repository.
    pub(crate) fn warm_bookmarks_cache(&self) -> &Arc<WarmBookmarksCache> {
        &self.repo.warm_bookmarks_cache
    }

    pub(crate) fn derive_changeset_info_enabled(&self) -> bool {
        self.blob_repo()
            .get_derived_data_config()
            .derived_data_types
            .contains(ChangesetInfo::NAME)
    }

    /// Look up a changeset specifier to find the canonical bonsai changeset
    /// ID for a changeset.
    pub async fn resolve_specifier(
        &self,
        specifier: ChangesetSpecifier,
    ) -> Result<Option<ChangesetId>, MononokeError> {
        let id = match specifier {
            ChangesetSpecifier::Bonsai(cs_id) => {
                let exists = self
                    .blob_repo()
                    .changeset_exists_by_bonsai(self.ctx.clone(), cs_id)
                    .compat()
                    .await?;
                match exists {
                    true => Some(cs_id),
                    false => None,
                }
            }
            ChangesetSpecifier::Hg(hg_cs_id) => {
                self.blob_repo()
                    .get_bonsai_from_hg(self.ctx.clone(), hg_cs_id)
                    .compat()
                    .await?
            }
            ChangesetSpecifier::Globalrev(rev) => {
                self.blob_repo()
                    .get_bonsai_from_globalrev(rev)
                    .compat()
                    .await?
            }
            ChangesetSpecifier::GitSha1(git_sha1) => {
                self.blob_repo()
                    .bonsai_git_mapping()
                    .get_bonsai_from_git_sha1(git_sha1)
                    .await?
            }
        };
        Ok(id)
    }

    /// Resolve a bookmark to a changeset.
    pub async fn resolve_bookmark(
        &self,
        bookmark: impl AsRef<str>,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let bookmark = BookmarkName::new(bookmark.as_ref())?;
        let mut cs_id = self.warm_bookmarks_cache().get(&bookmark);

        if cs_id.is_none() {
            // The bookmark wasn't in the warm bookmark cache.  Check
            // the blobrepo directly in case this is a bookmark that
            // has just been created.
            cs_id = self
                .blob_repo()
                .get_bonsai_bookmark(self.ctx.clone(), &bookmark)
                .compat()
                .await?;
        }

        Ok(cs_id.map(|cs_id| ChangesetContext::new(self.clone(), cs_id)))
    }

    /// Resolve a changeset id by its prefix
    pub async fn resolve_changeset_id_prefix(
        &self,
        prefix: ChangesetPrefixSpecifier,
    ) -> Result<ChangesetSpecifierPrefixResolution, MononokeError> {
        const MAX_LIMIT_AMBIGUOUS_IDS: usize = 10;
        let resolved = match prefix {
            ChangesetPrefixSpecifier::Hg(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.blob_repo()
                    .get_bonsai_hg_mapping()
                    .get_many_hg_by_prefix(
                        self.ctx.clone(),
                        self.blob_repo().get_repoid(),
                        prefix,
                        MAX_LIMIT_AMBIGUOUS_IDS,
                    )
                    .compat()
                    .await?,
            ),
            ChangesetPrefixSpecifier::Bonsai(prefix) => ChangesetSpecifierPrefixResolution::from(
                self.blob_repo()
                    .get_changesets_object()
                    .get_many_by_prefix(
                        self.ctx.clone(),
                        self.blob_repo().get_repoid(),
                        prefix,
                        MAX_LIMIT_AMBIGUOUS_IDS,
                    )
                    .compat()
                    .await?,
            ),
        };
        Ok(resolved)
    }

    /// Look up a changeset by specifier.
    pub async fn changeset(
        &self,
        specifier: ChangesetSpecifier,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let changeset = self
            .resolve_specifier(specifier)
            .await?
            .map(|cs_id| ChangesetContext::new(self.clone(), cs_id));
        Ok(changeset)
    }

    /// Get Mercurial ID for multiple changesets
    ///
    /// This is a more efficient version of:
    /// ```ignore
    /// let ids: Vec<ChangesetId> = ...;
    /// ids.into_iter().map(|id| {
    ///     let hg_id = repo
    ///         .changeset(ChangesetSpecifier::Bonsai(id))
    ///         .await
    ///         .hg_id();
    ///     (id, hg_id)
    /// });
    /// ```
    pub async fn changeset_hg_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, HgChangesetId)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .get_hg_bonsai_mapping(self.ctx.clone(), changesets)
            .compat()
            .await?
            .into_iter()
            .map(|(hg_cs_id, cs_id)| (cs_id, hg_cs_id))
            .collect();
        Ok(mapping)
    }

    /// Similar to changeset_hg_ids, but returning Git-SHA1s.
    pub async fn changeset_git_sha1s(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, GitSha1)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .bonsai_git_mapping()
            .get(changesets.into())
            .await?
            .into_iter()
            .map(|entry| (entry.bcs_id, entry.git_sha1))
            .collect();
        Ok(mapping)
    }

    /// Similar to changeset_hg_ids, but returning Globalrevs.
    pub async fn changeset_globalrev_ids(
        &self,
        changesets: Vec<ChangesetId>,
    ) -> Result<Vec<(ChangesetId, Globalrev)>, MononokeError> {
        let mapping = self
            .blob_repo()
            .get_bonsai_globalrev_mapping(changesets)
            .compat()
            .await?
            .into_iter()
            .collect();
        Ok(mapping)
    }

    /// Get a list of bookmarks.
    pub fn list_bookmarks(
        &self,
        include_scratch: bool,
        prefix: Option<String>,
        limit: Option<u64>,
    ) -> impl Stream<Item = (String, ChangesetId), Error = MononokeError> {
        if include_scratch {
            let prefix = match prefix.map(BookmarkPrefix::new) {
                Some(Ok(prefix)) => prefix,
                Some(Err(e)) => {
                    return stream::once(Err(MononokeError::InvalidRequest(format!(
                        "invalid bookmark prefix: {}",
                        e
                    ))))
                    .boxify()
                }
                None => {
                    return stream::once(Err(MononokeError::InvalidRequest(
                        "prefix required to list scratch bookmarks".to_string(),
                    )))
                    .boxify()
                }
            };
            let limit = match limit {
                Some(limit) => limit,
                None => {
                    return stream::once(Err(MononokeError::InvalidRequest(
                        "limit required to list scratch bookmarks".to_string(),
                    )))
                    .boxify()
                }
            };
            self.blob_repo()
                .get_bonsai_bookmarks_by_prefix_maybe_stale(self.ctx.clone(), &prefix, limit)
                .map(|(bookmark, cs_id)| (bookmark.into_name().into_string(), cs_id))
                .map_err(MononokeError::from)
                .boxify()
        } else {
            // TODO(mbthomas): honour `limit` for publishing bookmarks
            let prefix = prefix.unwrap_or_else(|| "".to_string());
            self.blob_repo()
                .get_bonsai_publishing_bookmarks_maybe_stale(self.ctx.clone())
                .filter_map(move |(bookmark, cs_id)| {
                    let name = bookmark.into_name().into_string();
                    if name.starts_with(&prefix) {
                        Some((name, cs_id))
                    } else {
                        None
                    }
                })
                .map_err(MononokeError::from)
                .boxify()
        }
    }

    /// Get a stack for the list of heads (up to the first public commit).
    ///
    /// Limit represents the max depth to go into the stacks.
    /// Algo is designed to minimize number of db queries.
    /// Missing changesets are skipped.
    pub async fn stack(
        &self,
        changesets: Vec<ChangesetId>,
        limit: usize,
    ) -> Result<Stack, MononokeError> {
        if limit == 0 {
            return Ok(Default::default());
        }

        // initialize visited
        let mut visited: HashSet<_> = changesets.iter().cloned().collect();

        let phases = self.blob_repo().get_phases();

        // get phases
        let public_phases = phases
            .get_public(self.ctx.clone(), changesets.clone(), false)
            .compat()
            .await?;

        // partition
        let (mut public, mut draft): (HashSet<_>, HashSet<_>) = changesets
            .into_iter()
            .partition(|cs_id| public_phases.contains(cs_id));

        // initialize the queue
        let mut queue: Vec<_> = draft.iter().cloned().collect();

        let mut level: usize = 1;

        while !queue.is_empty() && level < limit {
            // get the unique parents for all changesets in the queue & skip visited & update visited
            let parents: Vec<_> = self
                .blob_repo()
                .get_changesets_object()
                .get_many(self.ctx.clone(), self.blob_repo().get_repoid(), queue)
                .compat()
                .await?
                .into_iter()
                .map(|cs_entry| cs_entry.parents)
                .flatten()
                .filter(|cs_id| !visited.contains(cs_id))
                .unique()
                .collect();

            visited.extend(parents.iter().cloned());

            // get phases for the parents
            let public_phases = phases
                .get_public(self.ctx.clone(), parents.clone(), false)
                .compat()
                .await?;

            // partition
            let (new_public, new_draft): (Vec<_>, Vec<_>) = parents
                .into_iter()
                .partition(|cs_id| public_phases.contains(cs_id));

            // update queue and level
            queue = new_draft.clone();
            level = level + 1;

            // update draft & public
            public.extend(new_public.into_iter());
            draft.extend(new_draft.into_iter());
        }

        Ok(Stack { draft, public })
    }

    /// Get a Tree by id.  Returns `None` if the tree doesn't exist.
    pub async fn tree(&self, tree_id: TreeId) -> Result<Option<TreeContext>, MononokeError> {
        TreeContext::new_check_exists(self.clone(), tree_id).await
    }

    /// Get a File by id.  Returns `None` if the file doesn't exist.
    pub async fn file(&self, file_id: FileId) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Canonical(file_id)).await
    }

    /// Get a File by content sha-1.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha1(
        &self,
        hash: Sha1,
    ) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha1(hash))).await
    }

    /// Get a File by content sha-256.  Returns `None` if the file doesn't exist.
    pub async fn file_by_content_sha256(
        &self,
        hash: Sha256,
    ) -> Result<Option<FileContext>, MononokeError> {
        FileContext::new_check_exists(self.clone(), FetchKey::Aliased(Alias::Sha256(hash))).await
    }

    /// Get the equivalent changeset from another repo - it will sync it if needed
    pub async fn xrepo_commit_lookup(
        &self,
        other: &Self,
        specifier: ChangesetSpecifier,
    ) -> Result<Option<ChangesetContext>, MononokeError> {
        let commit_sync_repos = match &self.repo.commit_sync_config {
            Some(commit_sync_config) => CommitSyncRepos::new(
                self.blob_repo().clone(),
                other.blob_repo().clone(),
                &commit_sync_config,
            )?,
            None => {
                return Err(MononokeError::InvalidRequest(format!(
                    "Commits from {} are not configured to be remapped to another repo",
                    self.repo.name
                )));
            }
        };
        let changeset =
            self.resolve_specifier(specifier)
                .await?
                .ok_or(MononokeError::InvalidRequest(format!(
                    "unknown commit specifier {}",
                    specifier
                )))?;

        let commit_syncer =
            CommitSyncer::new(self.synced_commit_mapping().clone(), commit_sync_repos);

        let maybe_cs_id = commit_syncer.sync_commit(&self.ctx, changeset).await?;
        Ok(maybe_cs_id.map(|cs_id| ChangesetContext::new(other.clone(), cs_id)))
    }

    /// Get a write context to make changes to this repository.
    pub async fn write(self) -> Result<RepoWriteContext, MononokeError> {
        if !self.repo.service_config.permit_writes {
            return Err(MononokeError::InvalidRequest(String::from(
                "service writes are not enabled for this repo",
            )));
        }

        // Check the user is permitted to write to this repo.
        self.repo.check_acl(&self.ctx, "write")?;

        Ok(RepoWriteContext::new(self))
    }

    /// Get an HgRepoContext to access this repo's data in Mercurial-specific formats.
    pub fn hg(self) -> HgRepoContext {
        HgRepoContext::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fixtures::{linear, merge_even};

    #[fbinit::compat_test]
    async fn test_try_find_child(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = Repo::new_test(ctx.clone(), linear::getrepo(fb).await).await?;

        let ancestor = ChangesetId::from_str(
            "c9f9a2a39195a583d523a4e5f6973443caeb0c66a315d5bf7db1b5775c725310",
        )?;
        let descendant = ChangesetId::from_str(
            "7785606eb1f26ff5722c831de402350cf97052dc44bc175da6ac0d715a3dbbf6",
        )?;

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or(format_err!("didn't find child"))?;
        assert_eq!(
            child,
            ChangesetId::from_str(
                "98ef3234c2f1acdbb272715e8cfef4a6378e5443120677e0d87d113571280f79"
            )?
        );

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 1).await?;
        assert!(maybe_child.is_none());

        Ok(())
    }

    #[fbinit::compat_test]
    async fn test_try_find_child_merge(fb: FacebookInit) -> Result<(), Error> {
        let ctx = CoreContext::test_mock(fb);
        let repo = Repo::new_test(ctx.clone(), merge_even::getrepo(fb).await).await?;

        let ancestor = ChangesetId::from_str(
            "35fb4e0fb3747b7ca4d18281d059be0860d12407dc5dce5e02fb99d1f6a79d2a",
        )?;
        let descendant = ChangesetId::from_str(
            "567a25d453cafaef6550de955c52b91bf9295faf38d67b6421d5d2e532e5adef",
        )?;

        let maybe_child = repo.try_find_child(&ctx, ancestor, descendant, 100).await?;
        let child = maybe_child.ok_or(format_err!("didn't find child"))?;
        assert_eq!(child, descendant);
        Ok(())
    }
}
