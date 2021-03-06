/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use anyhow::{format_err, Error};
use blobrepo::BlobRepo;
use blobstore::{Blobstore, Loadable};
use cloned::cloned;
use context::CoreContext;
use derived_data::BonsaiDerived;
use futures::{compat::Future01CompatExt, future::TryFutureExt, FutureExt as NewFutureExt};
use futures_ext::{bounded_traversal::bounded_traversal_stream, BoxFuture, FutureExt};
use futures_old::{
    future,
    stream::{iter_ok, FuturesUnordered},
    Future, Stream,
};
use manifest::{Entry, ManifestOps};
use maplit::{hashmap, hashset};
use mononoke_types::{ChangesetId, FileUnodeId, MPath, ManifestUnodeId};
use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;
use std::sync::Arc;
use unodes::RootUnodeManifestId;

use crate::fastlog_impl::{fetch_fastlog_batch_by_unode_id, fetch_flattened};
use crate::mapping::{FastlogParent, RootFastlog};

/// Returns a full history of the given path starting from the given unode in BFS order.
///
/// This is the public API of this crate i.e. what clients should use if they want to
/// fetch the history.
///
/// Given a unode representing a commit-path `list_file_history` traverses commit history
/// in BFS order.
/// In order to do this it keeps:
///   - history_graph: commit graph that is constructed from fastlog data and represents
///                    'child(cs_id) -> parents(cs_id)' relationship
///   - starting node: changeset to start BFS graph traversal
///   - processed nodes: nodes from the previous BFS layer are used to construct next nodes
///   - visited: set that marks nodes already enqueued for BFS
/// For example, for this commit graph where some file is changed in every commit and E - start:
///
///      o E  - stage: 0        commit_graph: E -> D
///      |                                    D -> B, C
///      o D  - stage: 1                      C -> []
///     / \                                   B -> A
///  B o  o C - stage: 2                      A -> []
///    |
///    o A    - stage: 3
///
/// On each step of bounded_traversal_stream it processes all the nodes on the same depth (stage)
/// and streams them:
///   1 - prefetch parents for the already processed nodes
///   2 - construct new BFS layer from already processed nodes
///   3 - if there was no processed nodes - it's the first iteration - use starting node
///   4 - return the new nodes and contruct new state
/// The stream stops when there is nothing to return.
///
/// Why to pop all nodes on the same depth and not just one commit at a time?
/// Because if history contains merges and parents for more than one node on the current depth
/// haven't been fetched yet, we can fetch them at the same time using FuturesUnordered.
pub fn list_file_history(
    ctx: CoreContext,
    repo: BlobRepo,
    path: Option<MPath>,
    unode_entry: Entry<ManifestUnodeId, FileUnodeId>,
) -> impl Stream<Item = ChangesetId, Error = Error> {
    unode_entry
        .load(ctx.clone(), &repo.get_blobstore())
        .from_err()
        .map(move |unode| {
            let changeset_id = match unode {
                Entry::Tree(mf_unode) => mf_unode.linknode().clone(),
                Entry::Leaf(file_unode) => file_unode.linknode().clone(),
            };

            let history_graph = hashmap! { changeset_id.clone() => None };
            let visited = hashset! { changeset_id.clone() };

            bounded_traversal_stream(
                256,
                // starting point
                Some(TraversalState {
                    history_graph,
                    visited,
                    starting_node: Some(changeset_id),
                    processed_nodes: vec![],
                }),
                // unfold
                {
                    cloned!(ctx, path, repo);
                    move |TraversalState {
                              history_graph,
                              visited,
                              starting_node,
                              processed_nodes,
                          }| {
                        do_history_unfold(
                            ctx.clone(),
                            repo.clone(),
                            path.clone(),
                            starting_node,
                            processed_nodes,
                            visited,
                            history_graph,
                        )
                    }
                },
            )
            .map(|history| iter_ok(history))
            .flatten()
        })
        .flatten_stream()
}

/// Returns history for a given unode if it exists.
///
/// TODO(aida): This is no longer a public API, however APIServer still uses it.
/// Needs to be changed after APIServer will be deprecated.
pub fn prefetch_history(
    ctx: CoreContext,
    repo: BlobRepo,
    unode_entry: Entry<ManifestUnodeId, FileUnodeId>,
) -> impl Future<Item = Option<Vec<(ChangesetId, Vec<FastlogParent>)>>, Error = Error> {
    let blobstore: Arc<dyn Blobstore> = Arc::new(repo.get_blobstore());
    async move {
        let maybe_fastlog_batch =
            fetch_fastlog_batch_by_unode_id(&ctx, &blobstore, unode_entry).await?;
        match maybe_fastlog_batch {
            Some(fastlog_batch) => {
                let res = fetch_flattened(&fastlog_batch, ctx, blobstore)
                    .compat()
                    .await?;
                Ok(Some(res))
            }
            None => Ok(None),
        }
    }
    .boxed()
    .compat()
}

struct TraversalState {
    history_graph: HashMap<ChangesetId, Option<Vec<ChangesetId>>>,
    visited: HashSet<ChangesetId>,
    // node to start BFS graph traversal
    starting_node: Option<ChangesetId>,
    // nodes that were already used and needed to construct next BFS layer
    processed_nodes: Vec<ChangesetId>,
}

fn do_history_unfold(
    ctx: CoreContext,
    repo: BlobRepo,
    path: Option<MPath>,
    starting_node: Option<ChangesetId>,
    processed_nodes: Vec<ChangesetId>,
    mut visited: HashSet<ChangesetId>,
    // commit graph: changesets -> parents
    mut history_graph: HashMap<ChangesetId, Option<Vec<ChangesetId>>>,
) -> impl Future<Item = (Vec<ChangesetId>, Option<TraversalState>), Error = Error> {
    let mut prefetch_parents = vec![];
    for cs_id in &processed_nodes {
        if let Some(None) = history_graph.get(cs_id) {
            // parents haven't been fetched yet
            prefetch_parents.push(cs_id.clone());
        }
    }

    // if prefetch_parents is empty the function doesn't do anything and just returns an empty vector
    prefetch_unodes_for_changesets(ctx.clone(), repo.clone(), path.clone(), prefetch_parents).map(
        move |unode_batches| {
            // fill the commit graph
            for unode_batch in unode_batches {
                process_unode_batch(unode_batch, &mut history_graph);
            }

            // generate next BFS stage
            let mut next_to_yield = vec![];
            for cs_id in &processed_nodes {
                if let Some(Some(parents)) = history_graph.get(&cs_id) {
                    // parents are fetched, ready to process
                    for p in parents {
                        if visited.insert(*p) {
                            next_to_yield.push(*p);
                        }
                    }
                }
            }

            if next_to_yield.is_empty() {
                if let Some(node) = starting_node {
                    next_to_yield = vec![node];
                }
            }

            let new_state = if next_to_yield.is_empty() {
                None
            } else {
                Some(TraversalState {
                    history_graph,
                    visited,
                    starting_node: None,
                    // nodes that were just used are needed to generate the next BFS layer
                    processed_nodes: next_to_yield.clone(),
                })
            };
            (next_to_yield, new_state)
        },
    )
}

/// prefetches unode batches for each given changeset id
fn prefetch_unodes_for_changesets(
    ctx: CoreContext,
    repo: BlobRepo,
    path: Option<MPath>,
    changeset_ids: Vec<ChangesetId>,
) -> impl Future<Item = Vec<Vec<(ChangesetId, Vec<FastlogParent>)>>, Error = Error> {
    if changeset_ids.is_empty() {
        return future::ok(vec![]).left_future();
    }

    let prefetch_futs = changeset_ids.into_iter().map({
        cloned!(ctx, repo);
        move |cs_id| prefetch_history_by_changeset(ctx.clone(), repo.clone(), cs_id, path.clone())
    });

    FuturesUnordered::from_iter(prefetch_futs)
        .collect()
        .right_future()
}

fn process_unode_batch(
    unode_batch: Vec<(ChangesetId, Vec<FastlogParent>)>,
    graph: &mut HashMap<ChangesetId, Option<Vec<ChangesetId>>>,
) {
    for (cs_id, parents) in unode_batch {
        let has_unknown_parent = parents.iter().any(|parent| match parent {
            FastlogParent::Unknown => true,
            _ => false,
        });
        let known_parents: Vec<ChangesetId> = parents
            .into_iter()
            .filter_map(|parent| match parent {
                FastlogParent::Known(cs_id) => Some(cs_id),
                _ => None,
            })
            .collect();

        if let Some(maybe_parents) = graph.get(&cs_id) {
            // history graph has the changeset
            if maybe_parents.is_none() && !has_unknown_parent {
                // the node was visited but had unknown parents
                // let's update the graph
                graph.insert(cs_id, Some(known_parents.clone()));
            }
        } else {
            // we haven't seen this changeset before
            if has_unknown_parent {
                // at least one parent is unknown ->
                // need to fetch unode batch for this changeset
                //
                // let's add to the graph with None parents, this way we mark the
                // changeset as visited for other traversal branches
                graph.insert(cs_id, None);
            } else {
                graph.insert(cs_id, Some(known_parents.clone()));
            }
        }
    }
}

fn prefetch_history_by_changeset(
    ctx: CoreContext,
    repo: BlobRepo,
    changeset_id: ChangesetId,
    path: Option<MPath>,
) -> BoxFuture<Vec<(ChangesetId, Vec<FastlogParent>)>, Error> {
    cloned!(ctx, repo);
    let blobstore = repo.get_blobstore();
    RootUnodeManifestId::derive(ctx.clone(), repo.clone(), changeset_id.clone())
        .from_err()
        .and_then({
            cloned!(blobstore, ctx, path);
            move |root_unode_mf_id| {
                root_unode_mf_id
                    .manifest_unode_id()
                    .find_entry(ctx, blobstore, path)
            }
        })
        .and_then({
            cloned!(path);
            move |entry_opt| {
                entry_opt.ok_or_else(|| {
                    format_err!(
                        "Unode entry is not found {:?} {:?}",
                        changeset_id.clone(),
                        path,
                    )
                })
            }
        })
        .and_then({
            cloned!(ctx, repo, path);
            move |entry| {
                // optimistically try to fetch history for a unode
                prefetch_history(ctx.clone(), repo.clone(), entry).and_then({
                    move |maybe_history| match maybe_history {
                        Some(history) => future::ok(history).left_future(),
                        // if there is no history, let's try to derive batched fastlog data
                        // and fetch history again
                        None => RootFastlog::derive(ctx.clone(), repo.clone(), changeset_id)
                            .from_err()
                            .and_then({
                                cloned!(ctx, repo);
                                move |_| {
                                    prefetch_history(ctx.clone(), repo.clone(), entry).and_then(
                                        move |history_opt| {
                                            history_opt.ok_or_else(|| {
                                                format_err!(
                                                    "Fastlog data is not found {:?} {:?}",
                                                    changeset_id,
                                                    path
                                                )
                                            })
                                        },
                                    )
                                }
                            })
                            .right_future(),
                    }
                })
            }
        })
        .boxify()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mapping::RootFastlog;
    use blobrepo::save_bonsai_changesets;
    use blobrepo_factory::new_memblob_empty;
    use context::CoreContext;
    use fbinit::FacebookInit;
    use fixtures::{create_bonsai_changeset_with_files, store_files};
    use manifest::{Entry, ManifestOps};
    use maplit::btreemap;
    use mononoke_types::{ChangesetId, FileUnodeId, MPath, ManifestUnodeId};
    use std::collections::{HashMap, HashSet, VecDeque};
    use tokio_compat::runtime::Runtime;

    #[fbinit::test]
    fn test_list_linear_history(fb: FacebookInit) {
        // generate couple of hundreds linear file changes and list history
        let repo = new_memblob_empty(None).unwrap();
        let mut rt = Runtime::new().unwrap();
        let ctx = CoreContext::test_mock(fb);

        let filename = "1";
        let filepath = path(filename);

        let mut bonsais = vec![];
        let mut parents = vec![];
        let mut expected = vec![];
        for i in 1..300 {
            let file = if i % 2 == 1 { "2" } else { filename };
            let content = format!("{}", i);
            let stored_files = rt.block_on_std(store_files(
                ctx.clone(),
                btreemap! { file => Some(content.as_str()) },
                repo.clone(),
            ));

            let bcs = create_bonsai_changeset_with_files(parents, stored_files);
            let bcs_id = bcs.get_changeset_id();
            bonsais.push(bcs);
            if i % 2 != 1 {
                expected.push(bcs_id.clone());
            }
            parents = vec![bcs_id];
        }

        let latest = parents.get(0).unwrap().clone();
        rt.block_on(save_bonsai_changesets(bonsais, ctx.clone(), repo.clone()))
            .unwrap();

        let unode_entry = derive_and_get_unode_entry(
            ctx.clone(),
            repo.clone(),
            &mut rt,
            latest.clone(),
            filepath.clone(),
        );
        derive_fastlog(ctx.clone(), repo.clone(), &mut rt, latest);

        let history = rt
            .block_on(list_file_history(ctx.clone(), repo.clone(), filepath, unode_entry).collect())
            .unwrap();

        expected.reverse();
        assert_eq!(history, expected);
    }

    #[fbinit::test]
    fn test_list_history_with_merges(fb: FacebookInit) {
        // test generates commit graph with merges and compares result of list_file_history with
        // the result of BFS sorting on the graph
        //
        //           o - top
        //           |
        //           o - L+M
        //         / |
        //        o  o - L, M
        //         \ |
        //           o
        //           |
        //           :
        //           |
        //           o - A+B+C+D
        //           | \
        //     A+B - o  o
        //         / |  |
        //        o  o  o - C+D
        //        B  |  | \
        //           o  o  o
        //           |  |  |
        //           o  o  o
        //           |  C  D
        //           o
        //           A
        //

        let repo = new_memblob_empty(None).unwrap();
        let mut rt = Runtime::new().unwrap();
        let ctx = CoreContext::test_mock(fb);

        let filename = "1";
        let filepath = path(filename);

        let mut bonsais = vec![];
        let mut graph = HashMap::new();
        let mut create_branch = |branch, number, mut parents: Vec<_>| {
            for i in 0..number {
                let content = format!("{} - {}", branch, i);
                let stored_files = rt.block_on_std(store_files(
                    ctx.clone(),
                    btreemap! { filename => Some(content.as_str()) },
                    repo.clone(),
                ));

                let bcs = create_bonsai_changeset_with_files(parents.clone(), stored_files);
                let bcs_id = bcs.get_changeset_id();
                bonsais.push(bcs);

                graph.insert(bcs_id.clone(), parents);
                parents = vec![bcs_id];
            }
            parents.get(0).unwrap().clone()
        };

        let a_top = create_branch("A", 4, vec![]);
        let b_top = create_branch("B", 1, vec![]);
        let ab_top = create_branch("A+B", 1, vec![a_top, b_top]);

        let c_top = create_branch("C", 2, vec![]);
        let d_top = create_branch("D", 2, vec![]);
        let cd_top = create_branch("C+D", 2, vec![c_top, d_top]);

        let all_top = create_branch("A+B+C+D", 105, vec![ab_top, cd_top]);

        let l_top = create_branch("L", 1, vec![all_top.clone()]);
        let m_top = create_branch("M", 1, vec![all_top.clone()]);
        let top = create_branch("Top", 2, vec![l_top, m_top]);

        rt.block_on(save_bonsai_changesets(bonsais, ctx.clone(), repo.clone()))
            .unwrap();

        let unode_entry = derive_and_get_unode_entry(
            ctx.clone(),
            repo.clone(),
            &mut rt,
            top.clone(),
            filepath.clone(),
        );
        derive_fastlog(ctx.clone(), repo.clone(), &mut rt, top);

        let history = rt
            .block_on(list_file_history(ctx.clone(), repo.clone(), filepath, unode_entry).collect())
            .unwrap();

        let expected = bfs(&graph, top);
        assert_eq!(history, expected);
    }

    #[fbinit::test]
    fn test_list_history_many_diamonds(fb: FacebookInit) {
        // test generates commit graph with 50 diamonds
        //
        //              o - top
        //            /  \
        //           o    o
        //            \  /
        //             o
        //             |
        //             :
        //             |
        //             o
        //           /  \
        //          o    o
        //           \  /
        //            o
        //            |
        //            o - up
        //          /  \
        //  left - o    o - right
        //          \  /
        //           o - bottom
        //           |
        //           o
        //

        let repo = new_memblob_empty(None).unwrap();
        let mut rt = Runtime::new().unwrap();
        let ctx = CoreContext::test_mock(fb);

        let filename = "1";
        let filepath = path(filename);

        let create_changeset = |content: String, parents: Vec<_>| {
            let ctx = &ctx;
            let repo = &repo;
            async move {
                let stored_files = store_files(
                    ctx.clone(),
                    btreemap! { filename => Some(content.as_str()) },
                    repo.clone(),
                )
                .await;

                create_bonsai_changeset_with_files(parents, stored_files)
            }
        };

        let mut bonsais = vec![];
        let mut expected = vec![];

        let root = rt.block_on_std(create_changeset("root".to_string(), vec![]));
        let root_id = root.get_changeset_id();
        bonsais.push(root);
        expected.push(root_id.clone());

        let mut create_diamond = |number, parents: Vec<_>| {
            // bottom
            let bcs = rt.block_on_std(create_changeset(format!("B - {}", number), parents.clone()));
            let bottom_id = bcs.get_changeset_id();
            bonsais.push(bcs);
            expected.push(bottom_id.clone());

            // right
            let bcs = rt.block_on_std(create_changeset(format!("R - {}", number), vec![bottom_id]));
            let right_id = bcs.get_changeset_id();
            bonsais.push(bcs);
            expected.push(right_id.clone());

            // left
            let bcs = rt.block_on_std(create_changeset(format!("L - {}", number), vec![bottom_id]));
            let left_id = bcs.get_changeset_id();
            bonsais.push(bcs);
            expected.push(left_id.clone());

            // up
            let bcs = rt.block_on_std(create_changeset(
                format!("U - {}", number),
                vec![left_id, right_id],
            ));
            let up_id = bcs.get_changeset_id();
            bonsais.push(bcs);
            expected.push(up_id.clone());

            up_id
        };

        let mut prev_id = root_id;
        for i in 0..50 {
            prev_id = create_diamond(i, vec![prev_id]);
        }

        rt.block_on(save_bonsai_changesets(bonsais, ctx.clone(), repo.clone()))
            .unwrap();

        let unode_entry = derive_and_get_unode_entry(
            ctx.clone(),
            repo.clone(),
            &mut rt,
            prev_id.clone(),
            filepath.clone(),
        );
        derive_fastlog(ctx.clone(), repo.clone(), &mut rt, prev_id);

        let history = rt
            .block_on(list_file_history(ctx.clone(), repo.clone(), filepath, unode_entry).collect())
            .unwrap();

        expected.reverse();
        assert_eq!(history, expected);
    }

    fn bfs(graph: &HashMap<ChangesetId, Vec<ChangesetId>>, node: ChangesetId) -> Vec<ChangesetId> {
        let mut response = vec![];
        let mut queue = VecDeque::new();
        let mut visited = HashSet::new();
        queue.push_back(node.clone());
        visited.insert(node);

        while let Some(node) = queue.pop_front() {
            if let Some(parents) = graph.get(&node) {
                for p in parents {
                    if visited.insert(*p) {
                        queue.push_back(*p);
                    }
                }
            }
            response.push(node);
        }
        response
    }

    fn derive_and_get_unode_entry(
        ctx: CoreContext,
        repo: BlobRepo,
        rt: &mut Runtime,
        bcs_id: ChangesetId,
        path: Option<MPath>,
    ) -> Entry<ManifestUnodeId, FileUnodeId> {
        let root_unode = rt
            .block_on(RootUnodeManifestId::derive(
                ctx.clone(),
                repo.clone(),
                bcs_id.clone(),
            ))
            .unwrap();
        rt.block_on(root_unode.manifest_unode_id().find_entry(
            ctx.clone(),
            repo.get_blobstore(),
            path,
        ))
        .unwrap()
        .unwrap()
    }

    fn derive_fastlog(ctx: CoreContext, repo: BlobRepo, rt: &mut Runtime, bcs_id: ChangesetId) {
        rt.block_on(RootFastlog::derive(ctx, repo, bcs_id)).unwrap();
    }

    fn path(path_str: &str) -> Option<MPath> {
        MPath::new(path_str).ok()
    }
}
