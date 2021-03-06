/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use crate::BonsaiNodeStream;
use anyhow::{format_err, Error};
use blobrepo::BlobRepo;
use changeset_fetcher::ChangesetFetcher;
use context::CoreContext;
use futures_ext::{BoxFuture, FutureExt, StreamExt};
use futures_old::Future;
use mononoke_types::{ChangesetId, Generation};
use revset_test_helper::{single_changeset_id, string_to_bonsai};
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

pub struct TestChangesetFetcher {
    repo: Arc<BlobRepo>,
}

impl TestChangesetFetcher {
    pub fn new(repo: Arc<BlobRepo>) -> Self {
        Self { repo }
    }
}

impl ChangesetFetcher for TestChangesetFetcher {
    fn get_generation_number(
        &self,
        ctx: CoreContext,
        cs_id: ChangesetId,
    ) -> BoxFuture<Generation, Error> {
        self.repo
            .get_generation_number(ctx, cs_id)
            .and_then(move |genopt| genopt.ok_or_else(|| format_err!("{} not found", cs_id)))
            .boxify()
    }

    fn get_parents(
        &self,
        ctx: CoreContext,
        cs_id: ChangesetId,
    ) -> BoxFuture<Vec<ChangesetId>, Error> {
        self.repo
            .get_changeset_parents_by_bonsai(ctx, cs_id)
            .boxify()
    }

    fn get_stats(&self) -> HashMap<String, Box<dyn Any>> {
        HashMap::new()
    }
}
pub async fn get_single_bonsai_streams(
    ctx: CoreContext,
    repo: &Arc<BlobRepo>,
    hashes: &[&str],
) -> Vec<BonsaiNodeStream> {
    let mut ret = vec![];

    for hash in hashes {
        let stream = single_changeset_id(
            ctx.clone(),
            string_to_bonsai(ctx.fb, &repo.clone(), hash).await,
            &repo,
        )
        .boxify();
        ret.push(stream)
    }

    ret
}
