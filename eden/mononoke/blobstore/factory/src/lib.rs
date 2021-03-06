/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

use std::num::NonZeroU64;
use std::{path::PathBuf, sync::Arc};

use anyhow::Error;
use cloned::cloned;
use failure_ext::chain::ChainExt;
use fbinit::FacebookInit;
use futures::{
    future::{self, IntoFuture},
    Future,
};
use futures_ext::{try_boxfuture, BoxFuture, FutureExt};

use blobstore::ErrorKind;
use blobstore::{Blobstore, DisabledBlob};
use blobstore_sync_queue::SqlBlobstoreSyncQueue;
use chaosblob::ChaosBlobstore;
use fileblob::Fileblob;
use itertools::Either;
use manifoldblob::ThriftManifoldBlob;
use metaconfig_types::{
    self, BlobConfig, BlobstoreId, MetadataDBConfig, MultiplexId, ScrubAction,
    ShardedFilenodesParams,
};
use multiplexedblob::{LoggingScrubHandler, MultiplexedBlobstore, ScrubBlobstore, ScrubHandler};
use prefixblob::PrefixBlobstore;
use readonlyblob::ReadOnlyBlobstore;
use scuba::ScubaSampleBuilder;
use slog::Logger;
use sql_ext::{
    create_sqlite_connections,
    facebook::{
        create_myrouter_connections, create_raw_xdb_connections, myrouter_ready, FbSqlConstructors,
        MysqlOptions, PoolSizeConfig,
    },
    SqlConnections, SqlConstructors,
};
use sqlblob::Sqlblob;
//use sqlfilenodes::{SqlConstructors, SqlFilenodes};
use newfilenodes::NewFilenodesBuilder;
use throttledblob::ThrottledBlob;

#[derive(Copy, Clone, PartialEq)]
pub struct ReadOnlyStorage(pub bool);

#[derive(Copy, Clone, PartialEq)]
pub enum Scrubbing {
    Enabled,
    Disabled,
}

pub use chaosblob::ChaosOptions;
pub use throttledblob::ThrottleOptions;

#[derive(Clone, Debug)]
pub struct BlobstoreOptions {
    pub chaos_options: ChaosOptions,
    pub throttle_options: ThrottleOptions,
    pub manifold_api_key: Option<String>,
}

impl BlobstoreOptions {
    pub fn new(
        chaos_options: ChaosOptions,
        throttle_options: ThrottleOptions,
        manifold_api_key: Option<String>,
    ) -> Self {
        Self {
            chaos_options,
            throttle_options,
            manifold_api_key,
        }
    }
}

impl Default for BlobstoreOptions {
    fn default() -> Self {
        Self::new(
            ChaosOptions::new(None, None),
            ThrottleOptions::new(None, None),
            None,
        )
    }
}

trait SqlFactoryBase: Send + Sync {
    /// Open an arbitrary struct implementing SqlConstructors
    fn open<T: SqlConstructors>(&self) -> BoxFuture<Arc<T>, Error> {
        self.open_owned().map(|r| Arc::new(r)).boxify()
    }

    /// Open an arbitrary struct implementing SqlConstructors (without Arc)
    fn open_owned<T: SqlConstructors>(&self) -> BoxFuture<T, Error>;

    /// Open NewFilenodesBuilder, and return a tier name and the struct.
    fn open_filenodes(&self) -> BoxFuture<(String, NewFilenodesBuilder), Error>;

    /// Creates connections to the db.
    fn create_connections(&self, label: String) -> BoxFuture<SqlConnections, Error>;
}

struct XdbFactory {
    fb: FacebookInit,
    db_address: String,
    readonly: bool,
    mysql_options: MysqlOptions,
    sharded_filenodes: Option<ShardedFilenodesParams>,
}

impl XdbFactory {
    fn new(
        fb: FacebookInit,
        db_address: String,
        mysql_options: MysqlOptions,
        sharded_filenodes: Option<ShardedFilenodesParams>,
        readonly: bool,
    ) -> Self {
        XdbFactory {
            fb,
            db_address,
            readonly,
            mysql_options,
            sharded_filenodes,
        }
    }
}

impl SqlFactoryBase for XdbFactory {
    fn open_owned<T: SqlConstructors>(&self) -> BoxFuture<T, Error> {
        T::with_xdb(
            self.fb,
            self.db_address.clone(),
            self.mysql_options,
            self.readonly,
        )
        .boxify()
    }

    fn open_filenodes(&self) -> BoxFuture<(String, NewFilenodesBuilder), Error> {
        let (tier, filenodes) = match self.sharded_filenodes.clone() {
            Some(ShardedFilenodesParams {
                shard_map,
                shard_num,
            }) => {
                let builder = NewFilenodesBuilder::with_sharded_xdb(
                    self.fb,
                    shard_map.clone(),
                    self.mysql_options,
                    shard_num.into(),
                    self.readonly,
                );
                (shard_map, builder)
            }
            None => {
                let builder = NewFilenodesBuilder::with_xdb(
                    self.fb,
                    self.db_address.clone(),
                    self.mysql_options,
                    self.readonly,
                );
                (self.db_address.clone(), builder)
            }
        };

        filenodes.map(move |filenodes| (tier, filenodes)).boxify()
    }

    fn create_connections(&self, label: String) -> BoxFuture<SqlConnections, Error> {
        match self.mysql_options.myrouter_port {
            Some(mysql_options) => future::ok(create_myrouter_connections(
                self.db_address.clone(),
                None,
                mysql_options,
                self.mysql_options.myrouter_read_service_type(),
                PoolSizeConfig::for_regular_connection(),
                label,
                self.readonly,
            ))
            .boxify(),
            None => create_raw_xdb_connections(
                self.fb,
                self.db_address.clone(),
                self.mysql_options.db_locator_read_instance_requirement(),
                self.readonly,
            )
            .boxify(),
        }
    }
}

struct SqliteFactory {
    path: PathBuf,
    readonly: bool,
}

impl SqliteFactory {
    fn new(path: PathBuf, readonly: bool) -> Self {
        SqliteFactory { path, readonly }
    }
}

impl SqlFactoryBase for SqliteFactory {
    fn open_owned<T: SqlConstructors>(&self) -> BoxFuture<T, Error> {
        let r = try_boxfuture!(T::with_sqlite_path(
            self.path.join("sqlite_dbs"),
            self.readonly
        ));
        Ok(r).into_future().boxify()
    }

    fn open_filenodes(&self) -> BoxFuture<(String, NewFilenodesBuilder), Error> {
        NewFilenodesBuilder::with_sqlite_path(self.path.join("sqlite_dbs"), self.readonly)
            .map(|filenodes| ("sqlite".to_string(), filenodes))
            .into_future()
            .boxify()
    }

    fn create_connections(&self, _label: String) -> BoxFuture<SqlConnections, Error> {
        create_sqlite_connections(&self.path.join("sqlite_dbs"), self.readonly)
            .into_future()
            .boxify()
    }
}

pub struct SqlFactory {
    underlying: Either<SqliteFactory, XdbFactory>,
}

impl SqlFactory {
    pub fn open<T: SqlConstructors>(&self) -> BoxFuture<Arc<T>, Error> {
        self.underlying.as_ref().either(|l| l.open(), |r| r.open())
    }

    pub fn open_owned<T: SqlConstructors>(&self) -> BoxFuture<T, Error> {
        self.underlying
            .as_ref()
            .either(|l| l.open_owned(), |r| r.open_owned())
    }

    pub fn open_filenodes(&self) -> BoxFuture<(String, NewFilenodesBuilder), Error> {
        self.underlying
            .as_ref()
            .either(|l| l.open_filenodes(), |r| r.open_filenodes())
    }

    pub fn create_connections(&self, label: String) -> BoxFuture<SqlConnections, Error> {
        self.underlying.as_ref().either(
            {
                cloned!(label);
                move |l| l.create_connections(label)
            },
            |r| r.create_connections(label),
        )
    }
}

pub fn make_sql_factory(
    fb: FacebookInit,
    dbconfig: MetadataDBConfig,
    mysql_options: MysqlOptions,
    readonly: ReadOnlyStorage,
    logger: Logger,
) -> impl Future<Item = SqlFactory, Error = Error> {
    match dbconfig {
        MetadataDBConfig::LocalDB { path } => {
            let sql_factory = SqliteFactory::new(path.to_path_buf(), readonly.0);
            future::ok(SqlFactory {
                underlying: Either::Left(sql_factory),
            })
            .left_future()
        }
        MetadataDBConfig::Mysql {
            db_address,
            sharded_filenodes,
        } => {
            let sql_factory = XdbFactory::new(
                fb,
                db_address.clone(),
                mysql_options,
                sharded_filenodes,
                readonly.0,
            );
            myrouter_ready(Some(db_address), mysql_options, logger)
                .map(move |()| SqlFactory {
                    underlying: Either::Right(sql_factory),
                })
                .right_future()
        }
    }
}

/// Construct a blobstore according to the specification. The multiplexed blobstore
/// needs an SQL DB for its queue, as does the MySQL blobstore.
/// If `throttling.read_qps` or `throttling.write_qps` are Some then ThrottledBlob will be used to limit
/// QPS to the underlying blobstore
pub fn make_blobstore(
    fb: FacebookInit,
    blobconfig: BlobConfig,
    mysql_options: MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    blobstore_options: BlobstoreOptions,
    logger: Logger,
) -> BoxFuture<Arc<dyn Blobstore>, Error> {
    use BlobConfig::*;
    let mut has_components = false;
    let store = match blobconfig {
        Disabled => {
            Ok(Arc::new(DisabledBlob::new("Disabled by configuration")) as Arc<dyn Blobstore>)
                .into_future()
                .boxify()
        }

        Files { path } => Fileblob::create(path.join("blobs"))
            .chain_err(ErrorKind::StateOpen)
            .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
            .map_err(Error::from)
            .into_future()
            .boxify(),

        Sqlite { path } => Sqlblob::with_sqlite_path(path.join("blobs"), readonly_storage.0)
            .chain_err(ErrorKind::StateOpen)
            .map_err(Error::from)
            .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
            .into_future()
            .boxify(),

        Manifold { bucket, prefix } => ThriftManifoldBlob::new(
            fb,
            bucket.clone(),
            blobstore_options.clone().manifold_api_key,
        )
        .map({
            cloned!(prefix);
            move |manifold| PrefixBlobstore::new(manifold, format!("flat/{}", prefix))
        })
        .chain_err(ErrorKind::StateOpen)
        .map_err(Error::from)
        .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
        .into_future()
        .boxify(),

        Mysql {
            shard_map,
            shard_num,
        } => if let Some(myrouter_port) = mysql_options.myrouter_port {
            Sqlblob::with_myrouter(
                fb,
                shard_map.clone(),
                myrouter_port,
                mysql_options.myrouter_read_service_type(),
                shard_num,
                readonly_storage.0,
            )
        } else {
            Sqlblob::with_raw_xdb_shardmap(
                fb,
                shard_map.clone(),
                mysql_options.db_locator_read_instance_requirement(),
                shard_num,
                readonly_storage.0,
            )
        }
        .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
        .into_future()
        .boxify(),
        Multiplexed {
            multiplex_id,
            scuba_table,
            scuba_sample_rate,
            blobstores,
            queue_db,
        } => {
            has_components = true;
            make_blobstore_multiplexed(
                fb,
                multiplex_id,
                queue_db,
                scuba_table,
                scuba_sample_rate,
                blobstores,
                mysql_options,
                readonly_storage,
                None,
                blobstore_options.clone(),
                logger,
            )
        }
        Scrub {
            multiplex_id,
            scuba_table,
            scuba_sample_rate,
            blobstores,
            scrub_action,
            queue_db,
        } => {
            has_components = true;
            make_blobstore_multiplexed(
                fb,
                multiplex_id,
                queue_db,
                scuba_table,
                scuba_sample_rate,
                blobstores,
                mysql_options,
                readonly_storage,
                Some((
                    Arc::new(LoggingScrubHandler::new(false)) as Arc<dyn ScrubHandler>,
                    scrub_action,
                )),
                blobstore_options.clone(),
                logger,
            )
        }
        ManifoldWithTtl {
            bucket,
            prefix,
            ttl,
        } => ThriftManifoldBlob::new_with_ttl(
            fb,
            bucket.clone(),
            ttl,
            blobstore_options.clone().manifold_api_key,
        )
        .map({
            cloned!(prefix);
            move |manifold| PrefixBlobstore::new(manifold, format!("flat/{}", prefix))
        })
        .chain_err(ErrorKind::StateOpen)
        .map_err(Error::from)
        .map(|store| Arc::new(store) as Arc<dyn Blobstore>)
        .into_future()
        .boxify(),
    };

    let store = if readonly_storage.0 {
        store
            .map(|inner| Arc::new(ReadOnlyBlobstore::new(inner)) as Arc<dyn Blobstore>)
            .boxify()
    } else {
        store
    };

    let store = if blobstore_options.throttle_options.has_throttle() {
        store
            .map({
                cloned!(blobstore_options);
                move |inner| {
                    Arc::new(ThrottledBlob::new(
                        inner,
                        blobstore_options.throttle_options.clone(),
                    )) as Arc<dyn Blobstore>
                }
            })
            .boxify()
    } else {
        store
    };

    // For stores with components only set chaos on their components
    let store = if !has_components && blobstore_options.chaos_options.has_chaos() {
        store
            .map(move |inner| {
                Arc::new(ChaosBlobstore::new(inner, blobstore_options.chaos_options))
                    as Arc<dyn Blobstore>
            })
            .boxify()
    } else {
        store
    };

    // NOTE: Do not add wrappers here that should only be added once per repository, since this
    // function will get called recursively for each member of a Multiplex! For those, use
    // RepoBlobstoreArgs::new instead.

    store
}

pub fn make_blobstore_multiplexed(
    fb: FacebookInit,
    multiplex_id: MultiplexId,
    queue_db: MetadataDBConfig,
    scuba_table: Option<String>,
    scuba_sample_rate: NonZeroU64,
    inner_config: Vec<(BlobstoreId, BlobConfig)>,
    mysql_options: MysqlOptions,
    readonly_storage: ReadOnlyStorage,
    scrub_args: Option<(Arc<dyn ScrubHandler>, ScrubAction)>,
    blobstore_options: BlobstoreOptions,
    logger: Logger,
) -> BoxFuture<Arc<dyn Blobstore>, Error> {
    let component_readonly = match &scrub_args {
        // Need to write to components to repair them.
        Some((_, ScrubAction::Repair)) => ReadOnlyStorage(false),
        _ => readonly_storage,
    };

    let mut applied_chaos = false;
    let components: Vec<_> = inner_config
        .into_iter()
        .map({
            cloned!(logger);
            move |(blobstoreid, config)| {
                cloned!(blobstoreid, mut blobstore_options);
                if blobstore_options.chaos_options.has_chaos() {
                    if applied_chaos {
                        blobstore_options = BlobstoreOptions {
                            chaos_options: ChaosOptions::new(None, None),
                            ..blobstore_options
                        };
                    } else {
                        applied_chaos = true;
                    }
                }
                make_blobstore(
                    // force per line for easier merges
                    fb,
                    config,
                    mysql_options,
                    component_readonly,
                    blobstore_options,
                    logger.clone(),
                )
                .map({ move |store| (blobstoreid, store) })
            }
        })
        .collect();

    let queue = make_sql_factory(fb, queue_db, mysql_options, readonly_storage, logger)
        .and_then(|sql_factory| sql_factory.open::<SqlBlobstoreSyncQueue>());

    queue
        .and_then({
            move |queue| {
                future::join_all(components).map({
                    move |components| match scrub_args {
                        Some((scrub_handler, scrub_action)) => Arc::new(ScrubBlobstore::new(
                            multiplex_id,
                            components,
                            queue,
                            scuba_table.map_or(ScubaSampleBuilder::with_discard(), |table| {
                                ScubaSampleBuilder::new(fb, table)
                            }),
                            scuba_sample_rate,
                            scrub_handler,
                            scrub_action,
                        ))
                            as Arc<dyn Blobstore>,
                        None => Arc::new(MultiplexedBlobstore::new(
                            multiplex_id,
                            components,
                            queue,
                            scuba_table.map_or(ScubaSampleBuilder::with_discard(), |table| {
                                ScubaSampleBuilder::new(fb, table)
                            }),
                            scuba_sample_rate,
                        )) as Arc<dyn Blobstore>,
                    }
                })
            }
        })
        .boxify()
}
