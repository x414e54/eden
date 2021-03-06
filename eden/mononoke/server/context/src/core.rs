/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use fbinit::FacebookInit;
use scuba_ext::ScubaSampleBuilder;
use session_id::SessionId;
use slog::{o, Drain, Level, Logger};
use slog_glog_fmt::default_drain;
use sshrelay::SshEnvVars;
use tracing::TraceContext;

use crate::logging::{LoggingContainer, SamplingKey};
use crate::perf_counters::PerfCounters;
use crate::session::SessionContainer;

#[derive(Debug, Clone)]
pub struct CoreContext {
    pub fb: FacebookInit,
    session: SessionContainer,
    logging: LoggingContainer,
}

impl CoreContext {
    pub fn new_with_logger(fb: FacebookInit, logger: Logger) -> Self {
        let session = SessionContainer::new_with_defaults(fb);
        session.new_context(logger, ScubaSampleBuilder::with_discard())
    }

    pub fn test_mock(fb: FacebookInit) -> Self {
        let session = SessionContainer::new_with_defaults(fb);

        let drain = default_drain().filter_level(Level::Debug).ignore_res();
        let logger = Logger::root(drain, o![]);
        session.new_context(logger, ScubaSampleBuilder::with_discard())
    }

    pub fn clone_and_reset(&self) -> Self {
        self.session
            .new_context(self.logger().clone(), self.scuba().clone())
    }

    pub fn clone_and_sample(&self, sampling_key: SamplingKey) -> Self {
        Self {
            fb: self.fb,
            session: self.session.clone(),
            logging: self.logging.clone_and_sample(sampling_key),
        }
    }

    pub fn with_mutated_scuba(
        &self,
        sample: impl FnOnce(ScubaSampleBuilder) -> ScubaSampleBuilder,
    ) -> Self {
        self.session
            .new_context(self.logger().clone(), sample(self.scuba().clone()))
    }

    pub(crate) fn new_with_containers(
        fb: FacebookInit,
        logging: LoggingContainer,
        session: SessionContainer,
    ) -> Self {
        Self {
            fb,
            logging,
            session,
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session.session_id()
    }

    pub fn logger(&self) -> &Logger {
        &self.logging.logger()
    }

    pub fn sampling_key(&self) -> Option<&SamplingKey> {
        self.logging.sampling_key()
    }

    pub fn scuba(&self) -> &ScubaSampleBuilder {
        &self.logging.scuba()
    }

    pub fn perf_counters(&self) -> &PerfCounters {
        &self.logging.perf_counters()
    }

    pub fn trace(&self) -> &TraceContext {
        &self.session.trace()
    }

    pub fn user_unix_name(&self) -> &Option<String> {
        &self.session.user_unix_name()
    }

    pub fn source_hostname(&self) -> &Option<String> {
        &self.session.source_hostname()
    }

    pub fn ssh_env_vars(&self) -> &SshEnvVars {
        &self.session.ssh_env_vars()
    }

    #[cfg(not(fbcode_build))]
    pub fn trace_upload(&self) -> impl ::futures::Future<Item = (), Error = ::anyhow::Error> {
        ::futures::future::ok(())
    }

    pub fn session(&self) -> &SessionContainer {
        &self.session
    }
}
