/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#![deny(warnings)]

use anyhow::Error;
use async_trait::async_trait;
use blobrepo::BlobRepo;
use blobstore::Loadable;
use bookmarks::{BookmarkName, BookmarkUpdateReason};
use context::CoreContext;
use fbinit::FacebookInit;
use fixtures::many_files_dirs;
use futures::{
    compat::Future01CompatExt,
    future,
    stream::{futures_unordered, TryStreamExt},
};
use hooks::{
    hook_loader::load_hooks, ErrorKind, Hook, HookChangeset, HookChangesetParents, HookContext,
    HookExecution, HookFile, HookManager, HookRejectionInfo,
};
use hooks_content_stores::{
    BlobRepoChangesetStore, BlobRepoFileContentStore, ChangedFileType, InMemoryChangesetStore,
    InMemoryFileContentStore,
};
use maplit::{btreemap, hashmap, hashset};
use mercurial_types::{HgChangesetId, MPath};
use mercurial_types_mocks::nodehash::{ONES_FNID, THREES_FNID, TWOS_FNID};
use metaconfig_types::{
    BlobConfig, BookmarkParams, Bundle2ReplayParams, DerivedDataConfig, HookConfig, HookParams,
    HookType, InfinitepushParams, MetadataDBConfig, Redaction, RepoConfig, RepoReadOnly,
    SourceControlServiceParams, StorageConfig,
};
use mononoke_types::{FileType, RepositoryId};
use regex::Regex;
use scuba_ext::ScubaSampleBuilder;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use tests_utils::{create_commit, store_files};

#[derive(Clone, Debug)]
struct FnChangesetHook {
    f: fn(HookContext<HookChangeset>) -> HookExecution,
}

impl FnChangesetHook {
    fn new(f: fn(HookContext<HookChangeset>) -> HookExecution) -> FnChangesetHook {
        FnChangesetHook { f }
    }
}

#[async_trait]
impl Hook<HookChangeset> for FnChangesetHook {
    async fn run(
        &self,
        _ctx: &CoreContext,
        context: HookContext<HookChangeset>,
    ) -> Result<HookExecution, Error> {
        Ok((self.f)(context))
    }
}

fn always_accepting_changeset_hook() -> Box<dyn Hook<HookChangeset>> {
    let f: fn(HookContext<HookChangeset>) -> HookExecution = |_| HookExecution::Accepted;
    Box::new(FnChangesetHook::new(f))
}

fn always_rejecting_changeset_hook() -> Box<dyn Hook<HookChangeset>> {
    let f: fn(HookContext<HookChangeset>) -> HookExecution = |_| default_rejection();
    Box::new(FnChangesetHook::new(f))
}

#[derive(Clone, Debug)]
struct ContextMatchingChangesetHook {
    expected_context: HookContext<HookChangeset>,
}

#[async_trait]
impl Hook<HookChangeset> for ContextMatchingChangesetHook {
    async fn run(
        &self,
        _ctx: &CoreContext,
        context: HookContext<HookChangeset>,
    ) -> Result<HookExecution, Error> {
        assert_eq!(self.expected_context, context);
        Ok(HookExecution::Accepted)
    }
}

fn context_matching_changeset_hook(
    expected_context: HookContext<HookChangeset>,
) -> Box<dyn Hook<HookChangeset>> {
    Box::new(ContextMatchingChangesetHook { expected_context })
}

#[derive(Clone, Debug)]
struct FileContentMatchingChangesetHook {
    expected_content: HashMap<String, Option<String>>,
}

#[async_trait]
impl Hook<HookChangeset> for FileContentMatchingChangesetHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookChangeset>,
    ) -> Result<HookExecution, Error> {
        let futs = futures_unordered::FuturesUnordered::new();
        for file in context.data.files {
            let fut = async move {
                match self.expected_content.get(&file.path) {
                    Some(expected_content) => {
                        let content = file.file_text(ctx).await?;
                        let content =
                            content.map(|c| std::str::from_utf8(c.as_bytes()).unwrap().to_string());

                        Ok(match (content, expected_content.as_ref()) {
                            (Some(content), Some(expected_content)) => {
                                if content.contains(expected_content) {
                                    true
                                } else {
                                    false
                                }
                            }
                            (None, None) => true,
                            _ => false,
                        })
                    }
                    None => Ok(false),
                }
            };
            futs.push(fut);
        }
        let opt_item = futs
            .try_skip_while(|b: &bool| future::ok::<_, Error>(*b))
            .try_next()
            .await?;
        Ok(if opt_item.is_some() {
            default_rejection()
        } else {
            HookExecution::Accepted
        })
    }
}

fn file_text_matching_changeset_hook(
    expected_content: HashMap<String, Option<String>>,
) -> Box<dyn Hook<HookChangeset>> {
    Box::new(FileContentMatchingChangesetHook { expected_content })
}

#[derive(Clone, Debug)]
struct LengthMatchingChangesetHook {
    expected_lengths: HashMap<String, u64>,
}

#[async_trait]
impl Hook<HookChangeset> for LengthMatchingChangesetHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookChangeset>,
    ) -> Result<HookExecution, Error> {
        let futs = futures_unordered::FuturesUnordered::new();
        for file in context.data.files {
            let fut = async move {
                match self.expected_lengths.get(&file.path) {
                    Some(expected_length) => {
                        let expected_length = *expected_length;
                        let length = file.len(ctx).await?;
                        Ok(length == expected_length)
                    }
                    None => Ok(false),
                }
            };
            futs.push(fut);
        }
        let opt_item = futs
            .try_skip_while(|b: &bool| future::ok::<_, Error>(*b))
            .try_next()
            .await?;
        Ok(if opt_item.is_some() {
            default_rejection()
        } else {
            HookExecution::Accepted
        })
    }
}

fn length_matching_changeset_hook(
    expected_lengths: HashMap<String, u64>,
) -> Box<dyn Hook<HookChangeset>> {
    Box::new(LengthMatchingChangesetHook { expected_lengths })
}

#[derive(Clone, Debug)]
struct OtherFileMatchingChangesetHook {
    file_path: String,
    expected_content: Option<String>,
}

#[async_trait]
impl Hook<HookChangeset> for OtherFileMatchingChangesetHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookChangeset>,
    ) -> Result<HookExecution, Error> {
        let opt = context
            .data
            .file_text(ctx, self.file_path.clone())
            .await?
            .map(|content| std::str::from_utf8(content.as_bytes()).unwrap().to_string());
        Ok(if opt == self.expected_content {
            HookExecution::Accepted
        } else {
            default_rejection()
        })
    }
}

fn other_file_matching_changeset_hook(
    file_path: String,
    expected_content: Option<String>,
) -> Box<dyn Hook<HookChangeset>> {
    Box::new(OtherFileMatchingChangesetHook {
        file_path,
        expected_content,
    })
}

#[derive(Clone, Debug)]
struct FnFileHook {
    f: fn(HookContext<HookFile>) -> HookExecution,
}

impl FnFileHook {
    fn new(f: fn(HookContext<HookFile>) -> HookExecution) -> FnFileHook {
        FnFileHook { f }
    }
}

#[async_trait]
impl Hook<HookFile> for FnFileHook {
    async fn run(
        &self,
        _ctx: &CoreContext,
        context: HookContext<HookFile>,
    ) -> Result<HookExecution, Error> {
        Ok((self.f)(context))
    }
}

fn always_accepting_file_hook() -> Box<dyn Hook<HookFile>> {
    let f: fn(HookContext<HookFile>) -> HookExecution = |_| HookExecution::Accepted;
    Box::new(FnFileHook::new(f))
}

fn always_rejecting_file_hook() -> Box<dyn Hook<HookFile>> {
    let f: fn(HookContext<HookFile>) -> HookExecution = |_| default_rejection();
    Box::new(FnFileHook::new(f))
}

#[derive(Clone, Debug)]
struct PathMatchingFileHook {
    paths: HashSet<String>,
}

#[async_trait]
impl Hook<HookFile> for PathMatchingFileHook {
    async fn run(
        &self,
        _ctx: &CoreContext,
        context: HookContext<HookFile>,
    ) -> Result<HookExecution, Error> {
        Ok(if self.paths.contains(&context.data.path) {
            HookExecution::Accepted
        } else {
            default_rejection()
        })
    }
}

fn path_matching_file_hook(paths: HashSet<String>) -> Box<dyn Hook<HookFile>> {
    Box::new(PathMatchingFileHook { paths })
}

#[derive(Clone, Debug)]
struct FileContentMatchingFileHook {
    expected_content: Option<String>,
}

#[async_trait]
impl Hook<HookFile> for FileContentMatchingFileHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookFile>,
    ) -> Result<HookExecution, Error> {
        let content = context.data.file_text(ctx).await?;
        let content = content.map(|c| std::str::from_utf8(c.as_bytes()).unwrap().to_string());
        Ok(match (content, self.expected_content.as_ref()) {
            (Some(content), Some(expected_content)) => {
                if content.contains(expected_content) {
                    HookExecution::Accepted
                } else {
                    default_rejection()
                }
            }
            (None, None) => HookExecution::Accepted,
            _ => default_rejection(),
        })
    }
}

fn file_text_matching_file_hook(expected_content: Option<String>) -> Box<dyn Hook<HookFile>> {
    Box::new(FileContentMatchingFileHook { expected_content })
}

#[derive(Clone, Debug)]
struct IsSymLinkMatchingFileHook {
    is_symlink: bool,
}

#[async_trait]
impl Hook<HookFile> for IsSymLinkMatchingFileHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookFile>,
    ) -> Result<HookExecution, Error> {
        let file_type = context.data.file_type(ctx)?;
        let actual = match file_type {
            FileType::Symlink => true,
            _ => false,
        };
        Ok(if self.is_symlink == actual {
            HookExecution::Accepted
        } else {
            default_rejection()
        })
    }
}

fn is_symlink_matching_file_hook(is_symlink: bool) -> Box<dyn Hook<HookFile>> {
    Box::new(IsSymLinkMatchingFileHook { is_symlink })
}

#[derive(Clone, Debug)]
struct LengthMatchingFileHook {
    length: u64,
}

#[async_trait]
impl Hook<HookFile> for LengthMatchingFileHook {
    async fn run(
        &self,
        ctx: &CoreContext,
        context: HookContext<HookFile>,
    ) -> Result<HookExecution, Error> {
        let length = context.data.len(ctx).await?;
        Ok(if length == self.length {
            HookExecution::Accepted
        } else {
            default_rejection()
        })
    }
}

fn length_matching_file_hook(length: u64) -> Box<dyn Hook<HookFile>> {
    Box::new(LengthMatchingFileHook { length })
}

#[fbinit::test]
fn test_changeset_hook_accepted(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => always_accepting_changeset_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_rejected(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => always_rejecting_changeset_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => default_rejection()
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_mix(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => always_accepting_changeset_hook(),
            "hook2".to_string() => always_rejecting_changeset_hook(),
            "hook3".to_string() => always_accepting_changeset_hook(),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()]
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook3".to_string()],
        };
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted,
            "hook2".to_string() => default_rejection(),
            "hook3".to_string() => HookExecution::Accepted,
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_context(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let files = vec![
            ("dir1/subdir1/subsubdir1/file_1".to_string(), ONES_FNID),
            ("dir1/subdir1/subsubdir2/file_1".to_string(), TWOS_FNID),
            ("dir1/subdir1/subsubdir2/file_2".to_string(), THREES_FNID),
        ];
        let content_store = Arc::new(InMemoryFileContentStore::new());
        let cs_id = default_changeset_id();
        let hook_files = files
            .iter()
            .map(|(path, entry_id)| {
                HookFile::new(
                    path.clone(),
                    content_store.clone(),
                    cs_id,
                    ChangedFileType::Added,
                    Some((*entry_id, FileType::Regular)),
                )
            })
            .collect();
        let parents = HookChangesetParents::One("2f866e7e549760934e31bf0420a873f65100ad63".into());
        let reviewers_acl_checker = Arc::new(None);
        let data = HookChangeset::new(
            "Stanislau Hlebik <stash@fb.com>".into(),
            hook_files,
            "3".into(),
            parents,
            cs_id,
            content_store,
            reviewers_acl_checker,
        );
        let expected_context = HookContext {
            hook_name: "hook1".into(),
            config: Default::default(),
            data,
            bookmark: BookmarkName::new("bm1").unwrap(),
        };
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => context_matching_changeset_hook(expected_context)
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_other_file_text(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => other_file_matching_changeset_hook("dir1/subdir1/subsubdir1/file_1".to_string(), Some("elephants".to_string())),
            "hook2".to_string() => other_file_matching_changeset_hook("dir1/subdir1/subsubdir1/file_1".to_string(), Some("giraffes".to_string())),
            "hook3".to_string() => other_file_matching_changeset_hook("dir1/subdir1/subsubdir2/file_2".to_string(), Some("aardvarks".to_string())),
            "hook4".to_string() => other_file_matching_changeset_hook("no/such/path".to_string(), None),
            "hook5".to_string() => other_file_matching_changeset_hook("no/such/path".to_string(), Some("whateva".to_string())),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string(), "hook3".to_string()]
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook3".to_string(), "hook4".to_string(), "hook5".to_string()]
        };
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted,
            "hook2".to_string() => default_rejection(),
            "hook3".to_string() => default_rejection(),
            "hook4".to_string() => HookExecution::Accepted,
            "hook5".to_string() => default_rejection(),
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_file_text(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hook1_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => Some("elephants".to_string()),
            "dir1/subdir1/subsubdir2/file_1".to_string() => Some("hippopatami".to_string()),
            "dir1/subdir1/subsubdir2/file_2".to_string() => Some("eels".to_string()),
        ];
        let hook2_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => Some("anteaters".to_string()),
            "dir1/subdir1/subsubdir2/file_1".to_string() => Some("hippopatami".to_string()),
            "dir1/subdir1/subsubdir2/file_2".to_string() => Some("eels".to_string()),
        ];
        let hook3_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => Some("anteaters".to_string()),
            "dir1/subdir1/subsubdir2/file_1".to_string() => Some("giraffes".to_string()),
            "dir1/subdir1/subsubdir2/file_2".to_string() => Some("lions".to_string()),
        ];
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => file_text_matching_changeset_hook(hook1_map),
            "hook2".to_string() => file_text_matching_changeset_hook(hook2_map),
            "hook3".to_string() => file_text_matching_changeset_hook(hook3_map),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()]
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook2".to_string(), "hook3".to_string()]
        };
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted,
            "hook2".to_string() => default_rejection(),
            "hook3".to_string() => default_rejection(),
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_changeset_hook_lengths(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hook1_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => 9,
            "dir1/subdir1/subsubdir2/file_1".to_string() => 11,
            "dir1/subdir1/subsubdir2/file_2".to_string() => 4
        ];
        let hook2_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => 9,
            "dir1/subdir1/subsubdir2/file_1".to_string() => 12,
            "dir1/subdir1/subsubdir2/file_2".to_string() => 4
        ];
        let hook3_map = hashmap![
            "dir1/subdir1/subsubdir1/file_1".to_string() => 15,
            "dir1/subdir1/subsubdir2/file_1".to_string() => 17,
            "dir1/subdir1/subsubdir2/file_2".to_string() => 2
        ];
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => length_matching_changeset_hook(hook1_map),
            "hook2".to_string() => length_matching_changeset_hook(hook2_map),
            "hook3".to_string() => length_matching_changeset_hook(hook3_map),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()],
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook3".to_string()],
        };
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted,
            "hook2".to_string() => default_rejection(),
            "hook3".to_string() => default_rejection(),
        };
        run_changeset_hooks(ctx, "bm1", hooks, bookmarks, regexes, expected).await;
    });
}

#[fbinit::test]
fn test_file_hook_accepted(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => always_accepting_file_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            }
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hook_rejected(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => always_rejecting_file_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            }
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hook_mix(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => always_rejecting_file_hook(),
            "hook2".to_string() => always_accepting_file_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook2".to_string()]
        };
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook2".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            }
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hooks_paths(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let matching_paths = hashset![
            "dir1/subdir1/subsubdir2/file_1".to_string(),
            "dir1/subdir1/subsubdir2/file_2".to_string(),
        ];
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => path_matching_file_hook(matching_paths),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            }
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hooks_paths_mix(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let matching_paths1 = hashset![
            "dir1/subdir1/subsubdir2/file_1".to_string(),
            "dir1/subdir1/subsubdir2/file_2".to_string(),
        ];
        let matching_paths2 = hashset!["dir1/subdir1/subsubdir1/file_1".to_string(),];
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => path_matching_file_hook(matching_paths1),
            "hook2".to_string() => path_matching_file_hook(matching_paths2),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook2".to_string()]
        };
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            },
            "hook2".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            }
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hook_file_text(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => file_text_matching_file_hook(Some("elephants".to_string())),
            "hook2".to_string() => file_text_matching_file_hook(Some("hippopatami".to_string())),
            "hook3".to_string() => file_text_matching_file_hook(Some("eels".to_string()))
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string()],
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook3".to_string()],
        };
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook2".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook3".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            },
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hook_is_symlink(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => is_symlink_matching_file_hook(true),
            "hook2".to_string() => is_symlink_matching_file_hook(false),
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()],
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook2".to_string()],
        };
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook2".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            },
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hook_length(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => length_matching_file_hook("elephants".len() as u64),
            "hook2".to_string() => length_matching_file_hook("hippopatami".len() as u64),
            "hook3".to_string() => length_matching_file_hook("eels".len() as u64),
            "hook4".to_string() => length_matching_file_hook(999)
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string(), "hook2".to_string(), "hook3".to_string()],
        };
        let regexes = hashmap! {
            "b.*".to_string() => vec!["hook3".to_string(), "hook4".to_string()],
        };
        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook2".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => HookExecution::Accepted,
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
            "hook3".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => HookExecution::Accepted,
            },
            "hook4".to_string() => hashmap! {
                "dir1/subdir1/subsubdir1/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_1".to_string() => default_rejection(),
                "dir1/subdir1/subsubdir2/file_2".to_string() => default_rejection(),
            },
        };
        run_file_hooks(
            ctx,
            "bm1",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::InMemory,
        )
        .await;
    });
}

#[fbinit::test]
fn test_register_changeset_hooks(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut hook_manager = hook_manager_inmem(fb).await;
        let hook1 = always_accepting_changeset_hook();
        hook_manager.register_changeset_hook("hook1", hook1.into(), Default::default());
        let hook2 = always_accepting_changeset_hook();
        hook_manager.register_changeset_hook("hook2", hook2.into(), Default::default());

        let set = hook_manager.changeset_hook_names();
        assert_eq!(2, set.len());
        assert!(set.contains("hook1"));
        assert!(set.contains("hook1"));
    });
}

#[fbinit::test]
fn test_cs_hooks_with_blob_store(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        let hooks: HashMap<String, Box<dyn Hook<HookChangeset>>> = hashmap! {
            "hook1".to_string() => always_accepting_changeset_hook()
        };
        let bookmarks = hashmap! {
            "bm1".to_string() => vec!["hook1".to_string()]
        };
        let regexes = hashmap! {};
        let expected = hashmap! {
            "hook1".to_string() => HookExecution::Accepted
        };
        run_changeset_hooks_with_mgr(
            ctx.clone(),
            "bm1",
            hooks,
            bookmarks,
            regexes.clone(),
            expected,
            ContentStoreType::Blob(many_files_dirs::getrepo(ctx.fb).await),
        )
        .await;
    });
}

#[fbinit::test]
fn test_file_hooks_with_blob_store(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let ctx = CoreContext::test_mock(fb);
        // Create an init a repo
        let (repo, hg_cs_id) = {
            let repo = blobrepo_factory::new_memblob_empty(None).unwrap();

            let parent = create_commit(
                ctx.clone(),
                repo.clone(),
                vec![],
                store_files(
                    ctx.clone(),
                    btreemap! {"toremove" => Some("content")},
                    repo.clone(),
                )
                .await,
            )
            .await;
            let bcs_id = create_commit(
                ctx.clone(),
                repo.clone(),
                vec![parent],
                store_files(
                    ctx.clone(),
                    btreemap! {
                        "toremove" => None,
                        "newfile" => Some("newcontent"),
                        "dir/somefile" => Some("good"),
                    },
                    repo.clone(),
                )
                .await,
            )
            .await;

            let mut txn = repo.update_bookmark_transaction(ctx.clone());
            txn.force_set(
                &BookmarkName::new("master").unwrap(),
                bcs_id,
                BookmarkUpdateReason::TestMove {
                    bundle_replay_data: None,
                },
            )
            .unwrap();
            txn.commit().compat().await.unwrap();
            let hg_cs_id = repo
                .get_hg_from_bonsai_changeset(ctx.clone(), bcs_id)
                .compat()
                .await
                .unwrap();
            (repo, hg_cs_id)
        };

        let bookmarks = hashmap! {
            "master".to_string() => vec!["hook1".to_string()]
        };
        let hooks: HashMap<String, Box<dyn Hook<HookFile>>> = hashmap! {
            "hook1".to_string() => length_matching_file_hook(4),
        };
        let regexes = hashmap! {};

        let expected = hashmap! {
            "hook1".to_string() => hashmap! {
                "newfile".to_string() => default_rejection(),
                "dir/somefile".to_string() => HookExecution::Accepted,
            },
        };
        run_file_hooks_for_cs(
            ctx,
            "master",
            hooks,
            bookmarks,
            regexes,
            expected,
            ContentStoreType::Blob(repo),
            hg_cs_id,
        )
        .await;
    })
}

async fn run_changeset_hooks(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn Hook<HookChangeset>>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HookExecution>,
) {
    run_changeset_hooks_with_mgr(
        ctx,
        bookmark_name,
        hooks,
        bookmarks,
        regexes,
        expected,
        ContentStoreType::InMemory,
    )
    .await
}

async fn run_changeset_hooks_with_mgr(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn Hook<HookChangeset>>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HookExecution>,
    content_store_type: ContentStoreType,
) {
    let mut hook_manager = setup_hook_manager(ctx.fb, bookmarks, regexes, content_store_type).await;
    for (hook_name, hook) in hooks {
        hook_manager.register_changeset_hook(&hook_name, hook.into(), Default::default());
    }
    let res = hook_manager
        .run_hooks_for_bookmark(
            &ctx,
            vec![default_changeset_id()],
            &BookmarkName::new(bookmark_name).unwrap(),
            None,
        )
        .await
        .unwrap();
    let map: HashMap<String, HookExecution> = res
        .into_iter()
        .map(|outcome| (outcome.get_hook_name().to_string(), outcome.into()))
        .collect();
    assert_eq!(expected, map);
}

enum ContentStoreType {
    InMemory,
    Blob(BlobRepo),
}

async fn run_file_hooks(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn Hook<HookFile>>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HashMap<String, HookExecution>>,
    content_store_type: ContentStoreType,
) {
    run_file_hooks_with_mgr(
        ctx,
        bookmark_name,
        hooks,
        bookmarks,
        regexes,
        expected,
        content_store_type,
        default_changeset_id(),
    )
    .await
}

async fn run_file_hooks_for_cs(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn Hook<HookFile>>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HashMap<String, HookExecution>>,
    content_store_type: ContentStoreType,
    hg_cs_id: HgChangesetId,
) {
    run_file_hooks_with_mgr(
        ctx,
        bookmark_name,
        hooks,
        bookmarks,
        regexes,
        expected,
        content_store_type,
        hg_cs_id,
    )
    .await
}

async fn run_file_hooks_with_mgr(
    ctx: CoreContext,
    bookmark_name: &str,
    hooks: HashMap<String, Box<dyn Hook<HookFile>>>,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    expected: HashMap<String, HashMap<String, HookExecution>>,
    content_store_type: ContentStoreType,
    hg_cs_id: HgChangesetId,
) {
    let mut hook_manager = setup_hook_manager(ctx.fb, bookmarks, regexes, content_store_type).await;
    for (hook_name, hook) in hooks {
        hook_manager.register_file_hook(&hook_name, hook.into(), Default::default());
    }
    let res = hook_manager
        .run_hooks_for_bookmark(
            &ctx,
            vec![hg_cs_id],
            &BookmarkName::new(bookmark_name).unwrap(),
            None,
        )
        .await
        .unwrap();
    let map: HashMap<String, HashMap<String, HookExecution>> =
        res.into_iter().fold(HashMap::new(), |mut m, outcome| {
            let path = outcome.get_file_path().expect("Changeset hook").to_string();
            match m.entry(outcome.get_hook_name().to_string()) {
                Entry::Vacant(v) => v.insert(HashMap::new()).insert(path, outcome.into()),
                Entry::Occupied(mut v) => v.get_mut().insert(path, outcome.into()),
            };
            m
        });
    assert_eq!(expected, map);
}

async fn setup_hook_manager(
    fb: FacebookInit,
    bookmarks: HashMap<String, Vec<String>>,
    regexes: HashMap<String, Vec<String>>,
    content_store_type: ContentStoreType,
) -> HookManager {
    let mut hook_manager = match content_store_type {
        ContentStoreType::InMemory => hook_manager_inmem(fb).await,
        ContentStoreType::Blob(repo) => hook_manager_blobrepo(fb, repo),
    };
    for (bookmark_name, hook_names) in bookmarks {
        hook_manager
            .set_hooks_for_bookmark(BookmarkName::new(bookmark_name).unwrap().into(), hook_names);
    }
    for (regx, hook_names) in regexes {
        hook_manager.set_hooks_for_bookmark(Regex::new(&regx).unwrap().into(), hook_names);
    }
    hook_manager
}

fn default_rejection() -> HookExecution {
    HookExecution::Rejected(HookRejectionInfo::new_long(
        "desc".into(),
        "long_desc".to_string(),
    ))
}

fn default_changeset_id() -> HgChangesetId {
    HgChangesetId::from_str("d261bc7900818dea7c86935b3fb17a33b2e3a6b4").unwrap()
}

fn hook_manager_blobrepo(fb: FacebookInit, repo: BlobRepo) -> HookManager {
    let ctx = CoreContext::test_mock(fb);
    let changeset_store = BlobRepoChangesetStore::new(repo.clone());
    let content_store = BlobRepoFileContentStore::new(repo);
    HookManager::new(
        ctx.fb,
        Box::new(changeset_store),
        Arc::new(content_store),
        Default::default(),
        ScubaSampleBuilder::with_discard(),
    )
}

async fn hook_manager_many_files_dirs_blobrepo(fb: FacebookInit) -> HookManager {
    hook_manager_blobrepo(fb, many_files_dirs::getrepo(fb).await)
}

fn to_mpath(string: &str) -> MPath {
    // Please... avert your eyes
    MPath::new(string.to_string().as_bytes().to_vec()).unwrap()
}

async fn hook_manager_inmem(fb: FacebookInit) -> HookManager {
    let ctx = CoreContext::test_mock(fb);
    let repo = many_files_dirs::getrepo(fb).await;
    // Load up an in memory store with a single commit from the many_files_dirs store
    let cs_id = HgChangesetId::from_str("d261bc7900818dea7c86935b3fb17a33b2e3a6b4").unwrap();
    let cs = cs_id
        .load(ctx.clone(), repo.blobstore())
        .compat()
        .await
        .unwrap();
    let mut changeset_store = InMemoryChangesetStore::new();
    changeset_store.insert_changeset(cs_id, cs);
    let files = vec![
        (
            "dir1/subdir1/subsubdir1/file_1".to_string(),
            ChangedFileType::Added,
            Some((ONES_FNID, FileType::Symlink)),
        ),
        (
            "dir1/subdir1/subsubdir2/file_1".to_string(),
            ChangedFileType::Added,
            Some((TWOS_FNID, FileType::Regular)),
        ),
        (
            "dir1/subdir1/subsubdir2/file_2".to_string(),
            ChangedFileType::Added,
            Some((THREES_FNID, FileType::Regular)),
        ),
    ];
    changeset_store.insert_files(cs_id, files);

    let mut content_store = InMemoryFileContentStore::new();
    content_store.insert(
        cs_id.clone(),
        to_mpath("dir1/subdir1/subsubdir1/file_1"),
        ONES_FNID,
        "elephants",
    );
    content_store.insert(
        cs_id.clone(),
        to_mpath("dir1/subdir1/subsubdir2/file_1"),
        TWOS_FNID,
        "hippopatami",
    );
    content_store.insert(
        cs_id,
        to_mpath("dir1/subdir1/subsubdir2/file_2"),
        THREES_FNID,
        "eels",
    );

    HookManager::new(
        ctx.fb,
        Box::new(changeset_store),
        Arc::new(content_store),
        Default::default(),
        ScubaSampleBuilder::with_discard(),
    )
}

fn default_repo_config() -> RepoConfig {
    RepoConfig {
        storage_config: StorageConfig {
            blobstore: BlobConfig::Disabled,
            dbconfig: MetadataDBConfig::LocalDB {
                path: "/some/place".into(),
            },
        },
        write_lock_db_address: None,
        enabled: true,
        generation_cache_size: 1,
        repoid: RepositoryId::new(1),
        scuba_table: None,
        scuba_local_path: None,
        scuba_table_hooks: None,
        scuba_local_path_hooks: None,
        cache_warmup: None,
        hook_manager_params: None,
        bookmarks_cache_ttl: None,
        bookmarks: vec![],
        hooks: vec![],
        push: Default::default(),
        pushrebase: Default::default(),
        lfs: Default::default(),
        wireproto_logging: Default::default(),
        hash_validation_percentage: 0,
        readonly: RepoReadOnly::ReadWrite,
        redaction: Redaction::Enabled,
        skiplist_index_blobstore_key: None,
        bundle2_replay_params: Bundle2ReplayParams::default(),
        infinitepush: InfinitepushParams::default(),
        list_keys_patterns_max: 123,
        filestore: None,
        commit_sync_config: None,
        hook_max_file_size: 456,
        hipster_acl: None,
        source_control_service: SourceControlServiceParams::default(),
        source_control_service_monitoring: None,
        derived_data_config: DerivedDataConfig::default(),
    }
}

#[fbinit::test]
fn test_verify_integrity_fast_failure(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut config = default_repo_config();
        config.bookmarks = vec![BookmarkParams {
            bookmark: Regex::new("bm2").unwrap().into(),
            hooks: vec!["rust:verify_integrity".into()],
            only_fast_forward: false,
            allowed_users: None,
            rewrite_dates: None,
        }];
        config.hooks = vec![HookParams {
            name: "rust:verify_integrity".into(),
            hook_type: HookType::PerChangeset,
            config: HookConfig {
                strings: hashmap! {String::from("verify_integrity_path") => String::from("bad_nonexisting_filename")},
                ..Default::default()
            },
        }];

        let mut hm = hook_manager_many_files_dirs_blobrepo(fb).await;
        load_hooks(fb, &mut hm, config, &hashset![])
            .expect_err("`verify_integrity` hook loading should have failed");
    });
}

#[fbinit::test]
fn test_load_hooks_bad_rust_hook(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut config = default_repo_config();
        config.bookmarks = vec![BookmarkParams {
            bookmark: BookmarkName::new("bm1").unwrap().into(),
            hooks: vec!["rust:hook1".into()],
            only_fast_forward: false,
            allowed_users: None,
            rewrite_dates: None,
        }];

        config.hooks = vec![HookParams {
            name: "rust:hook1".into(),
            hook_type: HookType::PerChangeset,
            config: Default::default(),
        }];

        let mut hm = hook_manager_many_files_dirs_blobrepo(fb).await;

        match load_hooks(fb, &mut hm, config, &hashset![])
            .unwrap_err()
            .downcast::<ErrorKind>()
        {
            Ok(ErrorKind::InvalidRustHook(hook_name)) => {
                assert_eq!(hook_name, "rust:hook1".to_string());
            }
            _ => assert!(false, "Unexpected err type"),
        };
    });
}

#[fbinit::test]
fn test_load_disabled_hooks(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut config = default_repo_config();

        config.bookmarks = vec![];

        config.hooks = vec![HookParams {
            name: "hook1".into(),
            hook_type: HookType::PerChangeset,
            config: Default::default(),
        }];

        let mut hm = hook_manager_many_files_dirs_blobrepo(fb).await;

        load_hooks(fb, &mut hm, config, &hashset!["hook1".to_string()])
            .expect("disabling a broken hook should allow loading to succeed");
    });
}

#[fbinit::test]
fn test_load_disabled_hooks_referenced_by_bookmark(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut config = default_repo_config();

        config.bookmarks = vec![BookmarkParams {
            bookmark: BookmarkName::new("bm1").unwrap().into(),
            hooks: vec!["hook1".into()],
            only_fast_forward: false,
            allowed_users: None,
            rewrite_dates: None,
        }];

        config.hooks = vec![HookParams {
            name: "hook1".into(),
            hook_type: HookType::PerChangeset,
            config: Default::default(),
        }];

        let mut hm = hook_manager_many_files_dirs_blobrepo(fb).await;

        load_hooks(fb, &mut hm, config, &hashset!["hook1".to_string()])
            .expect("disabling a broken hook should allow loading to succeed");
    });
}

#[fbinit::test]
fn test_load_disabled_hooks_hook_does_not_exist(fb: FacebookInit) {
    async_unit::tokio_unit_test(async move {
        let mut config = default_repo_config();

        config.bookmarks = vec![];
        config.hooks = vec![];

        let mut hm = hook_manager_many_files_dirs_blobrepo(fb).await;

        match load_hooks(fb, &mut hm, config, &hashset!["hook1".to_string()])
            .unwrap_err()
            .downcast::<ErrorKind>()
        {
            Ok(ErrorKind::NoSuchHookToDisable(hooks)) => {
                assert_eq!(hashset!["hook1".to_string()], hooks);
            }
            _ => assert!(false, "Unexpected err type"),
        };
    });
}
