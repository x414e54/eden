/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use crate::errors::ErrorKind;
use anyhow::{bail, Error, Result};
use blobrepo::BlobRepo;
use blobstore::Loadable;
use bytes::Bytes;
use bytes_old::Bytes as BytesOld;
use cloned::cloned;
use context::{CoreContext, PerfCounterType};
use derived_data::BonsaiDerived;
use derived_data_filenodes::FilenodesOnlyPublic;
use filestore::FetchKey;
use futures::{
    compat::{Future01CompatExt, Stream01CompatExt},
    future::{self, FutureExt, TryFutureExt},
    stream::{self, Stream, StreamExt, TryStreamExt},
};
use futures_ext::{
    BoxFuture as OldBoxFuture, BoxStream as OldBoxStream, BufferedParams,
    FutureExt as OldFutureExt, StreamExt as OldStreamExt,
};
use futures_old::{
    future as old_future, stream as old_stream, Future as OldFuture, Stream as OldStream,
};
use futures_util::try_join;
use load_limiter::Metric;
use manifest::{find_intersection_of_diffs, Entry};
use mercurial_bundles::{
    changegroup::CgVersion,
    part_encode::PartEncodeBuilder,
    parts::{self, FilenodeEntry},
};
use mercurial_revlog::{self, RevlogChangeset};
use mercurial_types::{
    blobs::{fetch_manifest_envelope, File},
    FileBytes, HgBlobNode, HgChangesetId, HgFileNodeId, HgManifestId, HgParents, HgPhase, MPath,
    RevFlags, NULL_CSID,
};
use mononoke_types::{hash::Sha256, ChangesetId, ContentId};
use phases::Phases;
use reachabilityindex::LeastCommonAncestorsHint;
use repo_blobstore::RepoBlobstore;
use revset::DifferenceOfUnionsOfAncestorsNodeStream;
use slog::debug;
use stats::prelude::*;
use std::{
    collections::{HashMap, HashSet},
    iter::FromIterator,
    sync::Arc,
};

mod errors;

pub const MAX_FILENODE_BYTES_IN_MEMORY: u64 = 100_000_000;

define_stats! {
    prefix = "mononoke.getbundle_response";
    manifests_returned: dynamic_timeseries("manifests_returned.{}", (reponame: String); Rate, Sum),
    filenodes_returned: dynamic_timeseries("filenodes_returned.{}", (reponame: String); Rate, Sum),
    filenodes_weight: dynamic_timeseries("filesnodes_weight.{}", (reponame: String); Rate, Sum),
}

#[derive(PartialEq, Eq)]
pub enum PhasesPart {
    Yes,
    No,
}

/// An enum to identify the fullness of the information we
/// want to include into the `getbundle` response for draft
/// commits
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum DraftsInBundlesPolicy {
    /// Only include commit information (like for public changesets)
    CommitsOnly,
    /// Also include trees and files information
    WithTreesAndFiles,
}

#[derive(Clone)]
pub struct SessionLfsParams {
    pub threshold: Option<u64>,
}

pub async fn create_getbundle_response(
    ctx: CoreContext,
    blobrepo: BlobRepo,
    reponame: String,
    common: Vec<HgChangesetId>,
    heads: Vec<HgChangesetId>,
    lca_hint: Arc<dyn LeastCommonAncestorsHint>,
    return_phases: PhasesPart,
    lfs_params: SessionLfsParams,
    drafts_in_bundles_policy: DraftsInBundlesPolicy,
) -> Result<Vec<PartEncodeBuilder>, Error> {
    let return_phases = return_phases == PhasesPart::Yes;
    debug!(ctx.logger(), "Return phases is: {:?}", return_phases);

    let heads_len = heads.len();
    let common: HashSet<_> = common.into_iter().collect();
    let commits_to_send = find_commits_to_send(&ctx, &blobrepo, &common, &heads, &lca_hint);

    let phases = async {
        // Calculate phases only for heads that will be sent back to client (i.e. only
        // for heads that are not in "common"). Note that this is different from
        // "phases" part below, where we want to return phases for all heads.
        let filtered_heads = heads.iter().filter(|head| !common.contains(&head));
        let phases = prepare_phases(&ctx, &blobrepo, filtered_heads, &blobrepo.get_phases())
            .compat()
            .await?;
        report_draft_commits(&ctx, phases.iter());
        derive_filenodes_for_public_heads(&ctx, &blobrepo, &common, &phases).await?;
        Ok(phases)
    };

    let (phases, commits_to_send) = try_join!(phases, commits_to_send)?;

    let mut parts = vec![];
    if heads_len != 0 {
        // no heads means bookmark-only pushrebase, and the client
        // does not expect a changegroup part in this case

        let draft_hg_cs_ids: Vec<HgChangesetId> = phases
            .iter()
            .filter_map(|(hg_cs_id, hg_phase)| {
                if HgPhase::Public == *hg_phase {
                    None
                } else {
                    Some(hg_cs_id)
                }
            })
            .cloned()
            .collect();

        let should_include_trees_and_files =
            drafts_in_bundles_policy == DraftsInBundlesPolicy::WithTreesAndFiles;
        let (maybe_manifests, maybe_filenodes): (Option<_>, Option<_>) =
            if should_include_trees_and_files {
                let (manifests, filenodes) =
                    get_manifests_and_filenodes(&ctx, &blobrepo, draft_hg_cs_ids, &lfs_params)
                        .await?;
                report_manifests_and_filenodes(&ctx, reponame, manifests.len(), filenodes.iter());
                (Some(manifests), Some(filenodes))
            } else {
                (None, None)
            };

        let cg_part = create_hg_changeset_part(
            &ctx,
            &blobrepo,
            commits_to_send.clone(),
            maybe_filenodes,
            &lfs_params,
        )
        .await?;
        parts.push(cg_part);

        if let Some(manifests) = maybe_manifests {
            let manifests_stream =
                create_manifest_entries_stream(ctx.clone(), blobrepo.get_blobstore(), manifests);
            let tp_part = parts::treepack_part(manifests_stream)?;

            parts.push(tp_part);
        }
    }

    // Phases part has to be after the changegroup part.
    if return_phases {
        let phases = prepare_phases(&ctx, &blobrepo, heads.iter(), &blobrepo.get_phases())
            .compat()
            .await?;

        parts.push(parts::phases_part(
            ctx.clone(),
            old_stream::iter_ok(phases),
        )?);
    }

    Ok(parts)
}

fn report_draft_commits<'a, I: IntoIterator<Item = &'a (HgChangesetId, HgPhase)>>(
    ctx: &CoreContext,
    commit_phases: I,
) {
    let num_drafts = commit_phases
        .into_iter()
        .filter(|(_, ref phase)| phase == &HgPhase::Draft)
        .count();
    debug!(
        ctx.logger(),
        "Getbundle returning {} draft commits", num_drafts
    );
    ctx.perf_counters()
        .add_to_counter(PerfCounterType::GetbundleNumDrafts, num_drafts as i64);
}

fn report_manifests_and_filenodes<
    'a,
    FIter: IntoIterator<Item = (&'a MPath, &'a Vec<PreparedFilenodeEntry>)>,
>(
    ctx: &CoreContext,
    reponame: String,
    num_manifests: usize,
    filenodes: FIter,
) {
    let mut num_filenodes: i64 = 0;
    let mut total_filenodes_weight: i64 = 0;
    for filenode in filenodes {
        num_filenodes += filenode.1.len() as i64;
        let total_weight_for_mpath = filenode
            .1
            .iter()
            .fold(0, |acc, item| acc + item.entry_weight_hint);
        total_filenodes_weight += total_weight_for_mpath as i64;
    }

    debug!(
        ctx.logger(),
        "Getbundle returning {} manifests", num_manifests
    );
    ctx.perf_counters()
        .add_to_counter(PerfCounterType::GetbundleNumManifests, num_manifests as i64);
    STATS::manifests_returned.add_value(num_manifests as i64, (reponame.clone(),));

    debug!(
        ctx.logger(),
        "Getbundle returning {} filenodes with total size {} bytes",
        num_filenodes,
        total_filenodes_weight
    );
    ctx.perf_counters()
        .add_to_counter(PerfCounterType::GetbundleNumFilenodes, num_filenodes);
    ctx.perf_counters().add_to_counter(
        PerfCounterType::GetbundleFilenodesTotalWeight,
        total_filenodes_weight,
    );
    STATS::filenodes_returned.add_value(num_filenodes, (reponame.clone(),));
    STATS::filenodes_weight.add_value(total_filenodes_weight, (reponame,));
}

async fn derive_filenodes_for_public_heads(
    ctx: &CoreContext,
    blobrepo: &BlobRepo,
    common_heads: &HashSet<HgChangesetId>,
    phases: &Vec<(HgChangesetId, HgPhase)>,
) -> Result<(), Error> {
    let mut to_derive_filenodes = vec![];
    for (hg_cs_id, phase) in phases {
        if !common_heads.contains(&hg_cs_id) && phase == &HgPhase::Public {
            to_derive_filenodes.push(*hg_cs_id);
        }
    }

    let to_derive_filenodes_bonsai =
        hg_to_bonsai_stream(&ctx, &blobrepo, to_derive_filenodes).await?;
    Ok(stream::iter(to_derive_filenodes_bonsai)
        .map(move |bcs_id| {
            FilenodesOnlyPublic::derive(ctx.clone(), blobrepo.clone(), bcs_id).compat()
        })
        .buffered(100)
        .try_for_each(|_derive| async { Ok(()) })
        .await?)
}

async fn find_commits_to_send(
    ctx: &CoreContext,
    blobrepo: &BlobRepo,
    common: &HashSet<HgChangesetId>,
    heads: &Vec<HgChangesetId>,
    lca_hint: &Arc<dyn LeastCommonAncestorsHint>,
) -> Result<Vec<ChangesetId>, Error> {
    if common.is_empty() {
        bail!("no 'common' heads specified. Pull will be very inefficient. Please use hg clone instead");
    }

    let common_heads: HashSet<_> = HashSet::from_iter(common.iter());

    let heads = hg_to_bonsai_stream(
        &ctx,
        &blobrepo,
        heads
            .iter()
            .filter(|head| !common_heads.contains(head))
            .cloned()
            .collect(),
    );

    let excludes = hg_to_bonsai_stream(
        &ctx,
        &blobrepo,
        common
            .iter()
            .map(|node| node.clone())
            .filter(|node| node.into_nodehash() != NULL_CSID.into_nodehash())
            .collect(),
    );

    let (heads, excludes) = try_join!(heads, excludes)?;

    let changeset_fetcher = blobrepo.get_changeset_fetcher();
    let nodes_to_send = DifferenceOfUnionsOfAncestorsNodeStream::new_with_excludes(
        ctx.clone(),
        &changeset_fetcher,
        lca_hint.clone(),
        heads,
        excludes,
    )
    .collect()
    .compat()
    .await?;

    ctx.session().bump_load(Metric::EgressCommits, 1.0);
    ctx.perf_counters().add_to_counter(
        PerfCounterType::GetbundleNumCommits,
        nodes_to_send.len() as i64,
    );

    Ok(nodes_to_send.into_iter().rev().collect())
}

async fn create_hg_changeset_part(
    ctx: &CoreContext,
    blobrepo: &BlobRepo,
    nodes_to_send: Vec<ChangesetId>,
    maybe_prepared_filenode_entries: Option<HashMap<MPath, Vec<PreparedFilenodeEntry>>>,
    lfs_params: &SessionLfsParams,
) -> Result<PartEncodeBuilder> {
    let map_chunk_size = 100;
    let load_buffer_size = 1000;

    let changelogentries = stream::iter(nodes_to_send)
        .chunks(map_chunk_size)
        .then({
            cloned!(ctx, blobrepo);
            move |bonsais| {
                cloned!(ctx, blobrepo);
                async move {
                    let mapping = blobrepo
                        .get_hg_bonsai_mapping(ctx.clone(), bonsais.clone())
                        .compat()
                        .await?
                        .into_iter()
                        .map(|(hg_cs_id, bonsai_cs_id)| (bonsai_cs_id, hg_cs_id))
                        .collect::<HashMap<_, _>>();

                    // We need to preserve ordering of the Bonsais for Mercurial on the client-side.

                    let ordered_mapping = bonsais
                        .into_iter()
                        .map(|bcs_id| {
                            let hg_cs_id = mapping.get(&bcs_id).ok_or_else(|| {
                                anyhow::format_err!("cs_id was missing from mapping: {:?}", bcs_id)
                            })?;
                            Ok((*hg_cs_id, bcs_id))
                        })
                        .collect::<Vec<_>>();

                    Result::<_, Error>::Ok(ordered_mapping)
                }
            }
        })
        .map_ok(|res| stream::iter(res))
        .try_flatten()
        .map({
            cloned!(ctx, blobrepo);
            move |res| {
                cloned!(ctx, blobrepo);
                async move {
                    match res {
                        Ok((hg_cs_id, _bcs_id)) => {
                            let cs = hg_cs_id
                                .load(ctx.clone(), blobrepo.blobstore())
                                .compat()
                                .await?;
                            Ok((hg_cs_id, cs))
                        }
                        Err(e) => Err(e),
                    }
                }
            }
        })
        .buffered(load_buffer_size)
        .and_then(|(hg_cs_id, cs)| async move {
            let node = hg_cs_id.into_nodehash();

            let revlogcs = RevlogChangeset::new_from_parts(
                cs.parents(),
                cs.manifestid(),
                cs.user().into(),
                cs.time().clone(),
                cs.extra().clone(),
                cs.files().into(),
                cs.comments().into(),
            );

            let mut v = Vec::new();
            mercurial_revlog::changeset::serialize_cs(&revlogcs, &mut v)?;

            Ok((
                node,
                HgBlobNode::new(Bytes::from(v), revlogcs.p1(), revlogcs.p2()),
            ))
        })
        .boxed()
        .compat();

    let maybe_filenode_entries = match maybe_prepared_filenode_entries {
        Some(prepared_filenode_entries) => Some(
            create_filenodes(ctx.clone(), blobrepo.clone(), prepared_filenode_entries).boxify(),
        ),
        None => None,
    };

    let cg_version = if lfs_params.threshold.is_some() {
        CgVersion::Cg3Version
    } else {
        CgVersion::Cg2Version
    };

    parts::changegroup_part(changelogentries, maybe_filenode_entries, cg_version)
}

async fn hg_to_bonsai_stream(
    ctx: &CoreContext,
    repo: &BlobRepo,
    nodes: Vec<HgChangesetId>,
) -> Result<Vec<ChangesetId>, Error> {
    stream::iter(nodes)
        .map({
            move |node| {
                repo.get_bonsai_from_hg(ctx.clone(), node)
                    .and_then(move |maybe_bonsai| {
                        maybe_bonsai.ok_or(ErrorKind::BonsaiNotFoundForHgChangeset(node).into())
                    })
                    .compat()
            }
        })
        .buffered(100)
        .try_collect()
        .await
}

/// Calculate phases for the heads.
/// If client is pulling non-public changesets phases for public roots should be included.
fn prepare_phases<'a>(
    ctx: &CoreContext,
    repo: &BlobRepo,
    heads: impl IntoIterator<Item = &'a HgChangesetId>,
    phases: &Arc<dyn Phases>,
) -> impl OldFuture<Item = Vec<(HgChangesetId, HgPhase)>, Error = Error> {
    // create 'bonsai changesetid' => 'hg changesetid' hash map that will be later used
    // heads that are not known by the server will be skipped
    let heads: Vec<_> = heads.into_iter().cloned().collect();
    repo.get_hg_bonsai_mapping(ctx.clone(), heads)
        .map(move |hg_bonsai_mapping| {
            hg_bonsai_mapping
                .into_iter()
                .map(|(hg_cs_id, bonsai)| (bonsai, hg_cs_id))
                .collect::<HashMap<ChangesetId, HgChangesetId>>()
        })
        .and_then({
            // calculate phases for the heads
            cloned!(ctx, phases);
            move |bonsai_node_mapping| {
                phases
                    .get_public(ctx, bonsai_node_mapping.keys().cloned().collect(), false)
                    .map(move |public| (public, bonsai_node_mapping))
            }
        })
        .and_then({
            cloned!(ctx, repo, phases);
            move |(public, bonsai_node_mapping)| {
                // select draft heads
                let drafts = bonsai_node_mapping
                    .keys()
                    .filter(|csid| !public.contains(csid))
                    .cloned()
                    .collect();

                // find the public roots for the draft heads
                calculate_public_roots(ctx.clone(), repo.clone(), drafts, phases)
                    .and_then({
                        cloned!(ctx);
                        move |bonsais| {
                            repo.get_hg_bonsai_mapping(ctx, bonsais.into_iter().collect::<Vec<_>>())
                        }
                    })
                    .map(move |public_roots| {
                        let phases = bonsai_node_mapping
                            .into_iter()
                            .map(move |(csid, hg_csid)| {
                                let phase = if public.contains(&csid) {
                                    HgPhase::Public
                                } else {
                                    HgPhase::Draft
                                };
                                (hg_csid, phase)
                            })
                            .chain(
                                public_roots
                                    .into_iter()
                                    .map(|(hg_csid, _)| (hg_csid, HgPhase::Public)),
                            )
                            .collect();
                        phases
                    })
            }
        })
}

/// Calculate public roots for the set of draft changesets
fn calculate_public_roots(
    ctx: CoreContext,
    repo: BlobRepo,
    drafts: HashSet<ChangesetId>,
    phases: Arc<dyn Phases>,
) -> impl OldFuture<Item = HashSet<ChangesetId>, Error = Error> {
    old_future::loop_fn(
        (drafts, HashSet::new(), HashSet::new()),
        move |(drafts, mut public, mut visited)| {
            if drafts.is_empty() {
                return old_future::ok(old_future::Loop::Break(public)).left_future();
            }

            old_stream::iter_ok(drafts)
                .map({
                    cloned!(repo, ctx);
                    move |csid| repo.get_changeset_parents_by_bonsai(ctx.clone(), csid)
                })
                .buffered(100)
                .collect()
                .map(move |parents| {
                    let parents: HashSet<_> = parents
                        .into_iter()
                        .flatten()
                        .filter(|csid| !visited.contains(csid))
                        .collect();
                    visited.extend(parents.iter().cloned());
                    (parents, visited)
                })
                .and_then({
                    cloned!(ctx, phases);
                    move |(parents, visited)| {
                        phases
                            .get_public(ctx, parents.iter().cloned().collect(), false)
                            .map(move |public_phases| (public_phases, parents, visited))
                    }
                })
                .and_then(|(public_phases, parents, visited)| {
                    // split by phase
                    let (new_public, new_drafts) = parents
                        .into_iter()
                        .partition(|csid| public_phases.contains(csid));
                    // update found public changests
                    public.extend(new_public);
                    // continue for the new drafts
                    old_future::ok(old_future::Loop::Continue((new_drafts, public, visited)))
                })
                .right_future()
        },
    )
}

pub enum FilenodeEntryContent {
    InlineV2(ContentId),
    InlineV3(ContentId),
    LfsV3(Sha256, u64),
}

pub struct PreparedFilenodeEntry {
    pub filenode: HgFileNodeId,
    pub linknode: HgChangesetId,
    pub parents: HgParents,
    pub metadata: Bytes,
    pub content: FilenodeEntryContent,
    /// This field represents the memory footprint of a single
    /// entry when streaming. For inline-stored entries, this is
    /// just the size of the contents, while for LFS this is a size
    /// of an LFS pointer. Of course, this does not have to be
    /// precise, as it just provides an estimate on when to stop
    /// buffering.
    pub entry_weight_hint: u64,
}

impl PreparedFilenodeEntry {
    async fn into_filenode(
        self,
        ctx: CoreContext,
        repo: BlobRepo,
    ) -> Result<(HgFileNodeId, HgChangesetId, HgBlobNode, Option<RevFlags>), Error> {
        let Self {
            filenode,
            linknode,
            parents,
            metadata,
            content,
            ..
        } = self;

        async fn fetch_and_wrap(
            ctx: CoreContext,
            repo: BlobRepo,
            content_id: ContentId,
        ) -> Result<FileBytes, Error> {
            let content = filestore::fetch_concat(repo.blobstore(), ctx, content_id)
                .compat()
                .await?;

            Ok(FileBytes(content))
        };

        let (blob, flags) = match content {
            FilenodeEntryContent::InlineV2(content_id) => {
                let bytes = fetch_and_wrap(ctx, repo, content_id).await?;
                (generate_inline_file(&bytes, parents, &metadata), None)
            }
            FilenodeEntryContent::InlineV3(content_id) => {
                let bytes = fetch_and_wrap(ctx, repo, content_id).await?;
                (
                    generate_inline_file(&bytes, parents, &metadata),
                    Some(RevFlags::REVIDX_DEFAULT_FLAGS),
                )
            }
            FilenodeEntryContent::LfsV3(oid, size) => (
                generate_lfs_file(oid, parents, size, &metadata)?,
                Some(RevFlags::REVIDX_EXTSTORED),
            ),
        };

        Ok((filenode, linknode, blob, flags))
    }

    pub fn maybe_get_lfs_pointer(&self) -> Option<(Sha256, u64)> {
        match self.content {
            FilenodeEntryContent::LfsV3(sha256, size) => Some((sha256.clone(), size)),
            _ => None,
        }
    }
}

fn calculate_content_weight_hint(content_size: u64, content: &FilenodeEntryContent) -> u64 {
    match content {
        // Approximate calculation for LFS:
        // - 34 bytes for GitVersion
        // - 32 bytes for Sha256
        // - 8 bytes for size
        // - 40 bytes for parents
        FilenodeEntryContent::LfsV3(_, _) => 34 + 32 + 8 + 40,
        // Approximate calculation for inline:
        // - content_size
        // - parents
        FilenodeEntryContent::InlineV2(_) | FilenodeEntryContent::InlineV3(_) => content_size + 40,
    }
}

fn prepare_filenode_entries_stream<'a>(
    ctx: &'a CoreContext,
    repo: &'a BlobRepo,
    filenodes: Vec<(MPath, HgFileNodeId, HgChangesetId)>,
    lfs_session: &'a SessionLfsParams,
) -> impl Stream<Item = Result<(MPath, Vec<PreparedFilenodeEntry>), Error>> + 'a {
    stream::iter(filenodes.into_iter())
        .map({
            move |(path, filenode, linknode)| async move {
                let envelope = filenode
                    .load(ctx.clone(), repo.blobstore())
                    .compat()
                    .await?;

                let file_size = envelope.content_size();

                let content = match lfs_session.threshold {
                    None => FilenodeEntryContent::InlineV2(envelope.content_id()),
                    Some(lfs_threshold) if file_size <= lfs_threshold => {
                        FilenodeEntryContent::InlineV3(envelope.content_id())
                    }
                    _ => {
                        let key = FetchKey::from(envelope.content_id());
                        let meta = filestore::get_metadata(repo.blobstore(), ctx.clone(), &key)
                            .compat()
                            .await?;
                        let meta =
                            meta.ok_or_else(|| Error::from(ErrorKind::MissingContent(key)))?;
                        let oid = meta.sha256;
                        FilenodeEntryContent::LfsV3(oid, file_size)
                    }
                };

                let parents = envelope.hg_parents();
                let entry_weight_hint = calculate_content_weight_hint(file_size, &content);
                let prepared_filenode_entry = PreparedFilenodeEntry {
                    filenode,
                    linknode,
                    parents,
                    metadata: envelope.metadata().clone(),
                    content,
                    entry_weight_hint,
                };

                Ok((path, vec![prepared_filenode_entry]))
            }
        })
        .buffered(100)
}

fn generate_inline_file(content: &FileBytes, parents: HgParents, metadata: &Bytes) -> HgBlobNode {
    let mut parents = parents.into_iter();
    let p1 = parents.next();
    let p2 = parents.next();

    // Metadata is only used to store copy/rename information
    let no_rename_metadata = metadata.is_empty();
    let mut res = vec![];
    res.extend(metadata);
    res.extend(content.as_bytes());
    if no_rename_metadata {
        HgBlobNode::new(Bytes::from(res), p1, p2)
    } else {
        // Mercurial has a complicated logic regarding storing renames
        // If copy/rename metadata is stored then p1 is always "null"
        // (i.e. hash like "00000000....") - that's why we set it to None below.
        // p2 is null for a non-merge commit, but not-null for merges.
        // (See D6922881 for more details about merge logic)
        //
        // It boils down to the fact that we can't have both p1 and p2 to be
        // non-null if we have rename metadata.
        // `HgFileEnvelope::hg_parents()` returns HgParents structure, which
        // always makes p2 a null commit if at least one parent commit is null.
        // And that's why we set the second parent to p1 below.
        debug_assert!(p2.is_none());
        HgBlobNode::new(Bytes::from(res), None, p1)
    }
}

fn generate_lfs_file(
    oid: Sha256,
    parents: HgParents,
    file_size: u64,
    metadata: &Bytes,
) -> Result<HgBlobNode, Error> {
    let copy_from = File::extract_copied_from(metadata)?;
    let bytes = File::generate_lfs_file(oid, file_size, copy_from)?;

    let mut parents = parents.into_iter();
    let p1 = parents.next();
    let p2 = parents.next();
    Ok(HgBlobNode::new(Bytes::from(bytes), p1, p2))
}

pub fn create_manifest_entries_stream(
    ctx: CoreContext,
    blobstore: RepoBlobstore,
    manifests: Vec<(Option<MPath>, HgManifestId, HgChangesetId)>,
) -> OldBoxStream<OldBoxFuture<parts::TreepackPartInput, Error>, Error> {
    old_stream::iter_ok(manifests.into_iter())
        .map({
            move |(fullpath, mf_id, linknode)| {
                fetch_manifest_envelope(ctx.clone(), &blobstore.boxed(), mf_id)
                    .map(move |mf_envelope| {
                        let (p1, p2) = mf_envelope.parents();
                        parts::TreepackPartInput {
                            node: mf_id.into_nodehash(),
                            p1,
                            p2,
                            content: BytesOld::from(mf_envelope.contents().as_ref()),
                            fullpath,
                            linknode: linknode.into_nodehash(),
                        }
                    })
                    .boxify()
            }
        })
        .boxify()
}

async fn diff_with_parents(
    ctx: CoreContext,
    repo: BlobRepo,
    hg_cs_id: HgChangesetId,
) -> Result<
    (
        Vec<(Option<MPath>, HgManifestId, HgChangesetId)>,
        Vec<(MPath, HgFileNodeId, HgChangesetId)>,
    ),
    Error,
> {
    let (mf_id, parent_mf_ids) = try_join!(fetch_manifest(ctx.clone(), &repo, &hg_cs_id), async {
        let parents = repo
            .get_changeset_parents(ctx.clone(), hg_cs_id)
            .compat()
            .await?;

        future::try_join_all(
            parents
                .iter()
                .map(|p| fetch_manifest(ctx.clone(), &repo, p)),
        )
        .await
    })?;

    let blobstore = Arc::new(repo.get_blobstore());
    let new_entries: Vec<(Option<MPath>, Entry<_, _>)> =
        find_intersection_of_diffs(ctx, blobstore, mf_id, parent_mf_ids)
            .compat()
            .try_collect()
            .await?;

    let mut mfs = vec![];
    let mut files = vec![];
    for (path, entry) in new_entries {
        match entry {
            Entry::Tree(mf) => {
                mfs.push((path, mf, hg_cs_id.clone()));
            }
            Entry::Leaf((_, file)) => {
                let path = path.expect("empty file paths?");
                files.push((path, file, hg_cs_id.clone()));
            }
        }
    }

    Ok((mfs, files))
}

fn create_filenodes_weighted(
    ctx: CoreContext,
    repo: BlobRepo,
    entries: HashMap<MPath, Vec<PreparedFilenodeEntry>>,
) -> impl OldStream<
    Item = (
        impl OldFuture<Item = (MPath, Vec<FilenodeEntry>), Error = Error>,
        u64,
    ),
    Error = Error,
> {
    let items = entries.into_iter().map({
        cloned!(ctx, repo);
        move |(path, prepared_entries)| {
            let total_weight: u64 = prepared_entries.iter().fold(0, |acc, prepared_entry| {
                acc + prepared_entry.entry_weight_hint
            });

            let entry_futs: Vec<_> = prepared_entries
                .into_iter()
                .map({
                    |entry| {
                        entry
                            .into_filenode(ctx.clone(), repo.clone())
                            .boxed()
                            .compat()
                    }
                })
                .collect();

            let fut = old_future::join_all(entry_futs).map(|entries| (path, entries));

            (fut, total_weight)
        }
    });
    old_stream::iter_ok(items)
}

pub fn create_filenodes(
    ctx: CoreContext,
    repo: BlobRepo,
    entries: HashMap<MPath, Vec<PreparedFilenodeEntry>>,
) -> impl OldStream<Item = (MPath, Vec<FilenodeEntry>), Error = Error> {
    let params = BufferedParams {
        weight_limit: MAX_FILENODE_BYTES_IN_MEMORY,
        buffer_size: 100,
    };
    create_filenodes_weighted(ctx, repo, entries).buffered_weight_limited(params)
}

pub async fn get_manifests_and_filenodes(
    ctx: &CoreContext,
    repo: &BlobRepo,
    commits: Vec<HgChangesetId>,
    lfs_params: &SessionLfsParams,
) -> Result<
    (
        Vec<(Option<MPath>, HgManifestId, HgChangesetId)>,
        HashMap<MPath, Vec<PreparedFilenodeEntry>>,
    ),
    Error,
> {
    let entries: Vec<_> = stream::iter(commits)
        .then({
            |hg_cs_id| async move {
                let (manifests, filenodes) =
                    diff_with_parents(ctx.clone(), repo.clone(), hg_cs_id).await?;

                let filenodes: Vec<(MPath, Vec<PreparedFilenodeEntry>)> =
                    prepare_filenode_entries_stream(&ctx, &repo, filenodes, &lfs_params)
                        .try_collect()
                        .await?;
                Result::<_, Error>::Ok((manifests, filenodes))
            }
        })
        .try_collect()
        .await?;

    let mut all_mf_entries = vec![];
    let mut all_filenode_entries: HashMap<_, Vec<_>> = HashMap::new();
    for (mf_entries, file_entries) in entries {
        all_mf_entries.extend(mf_entries);
        for (file_path, filenodes) in file_entries {
            all_filenode_entries
                .entry(file_path)
                .or_default()
                .extend(filenodes);
        }
    }

    Ok((all_mf_entries, all_filenode_entries))
}

async fn fetch_manifest(
    ctx: CoreContext,
    repo: &BlobRepo,
    hg_cs_id: &HgChangesetId,
) -> Result<HgManifestId, Error> {
    let blob_cs = hg_cs_id.load(ctx, repo.blobstore()).compat().await?;
    Ok(blob_cs.manifestid())
}
