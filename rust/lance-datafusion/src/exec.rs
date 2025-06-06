// SPDX-License-Identifier: Apache-2.0
// SPDX-FileCopyrightText: Copyright The Lance Authors

//! Utilities for working with datafusion execution plans

use std::sync::{Arc, Mutex};

use arrow_array::RecordBatch;
use arrow_schema::Schema as ArrowSchema;
use datafusion::{
    catalog::streaming::StreamingTable,
    dataframe::DataFrame,
    execution::{
        context::{SessionConfig, SessionContext},
        disk_manager::DiskManagerConfig,
        memory_pool::FairSpillPool,
        runtime_env::RuntimeEnvBuilder,
        TaskContext,
    },
    physical_plan::{
        analyze::AnalyzeExec,
        display::DisplayableExecutionPlan,
        execution_plan::{Boundedness, EmissionType},
        stream::RecordBatchStreamAdapter,
        streaming::PartitionStream,
        DisplayAs, DisplayFormatType, ExecutionPlan, PlanProperties, SendableRecordBatchStream,
    },
};
use datafusion_common::{DataFusionError, Statistics};
use datafusion_physical_expr::{EquivalenceProperties, Partitioning};
use lazy_static::lazy_static;

use futures::{stream, StreamExt};
use lance_arrow::SchemaExt;
use lance_core::{
    utils::{
        futures::FinallyStreamExt,
        tracing::{EXECUTION_PLAN_RUN, TRACE_EXECUTION},
    },
    Error, Result,
};
use log::{debug, info, warn};
use snafu::location;

use crate::utils::{
    MetricsExt, BYTES_READ_METRIC, INDEX_COMPARISONS_METRIC, INDICES_LOADED_METRIC, IOPS_METRIC,
    PARTS_LOADED_METRIC, REQUESTS_METRIC,
};

/// An source execution node created from an existing stream
///
/// It can only be used once, and will return the stream.  After that the node
/// is exhausted.
///
/// Note: the stream should be finite, otherwise we will report datafusion properties
/// incorrectly.
pub struct OneShotExec {
    stream: Mutex<Option<SendableRecordBatchStream>>,
    // We save off a copy of the schema to speed up formatting and so ExecutionPlan::schema & display_as
    // can still function after exhausted
    schema: Arc<ArrowSchema>,
    properties: PlanProperties,
}

impl OneShotExec {
    /// Create a new instance from a given stream
    pub fn new(stream: SendableRecordBatchStream) -> Self {
        let schema = stream.schema();
        Self {
            stream: Mutex::new(Some(stream)),
            schema: schema.clone(),
            properties: PlanProperties::new(
                EquivalenceProperties::new(schema),
                Partitioning::RoundRobinBatch(1),
                EmissionType::Incremental,
                Boundedness::Bounded,
            ),
        }
    }

    pub fn from_batch(batch: RecordBatch) -> Self {
        let schema = batch.schema();
        let stream = Box::pin(RecordBatchStreamAdapter::new(
            schema,
            stream::iter(vec![Ok(batch)]),
        ));
        Self::new(stream)
    }
}

impl std::fmt::Debug for OneShotExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let stream = self.stream.lock().unwrap();
        f.debug_struct("OneShotExec")
            .field("exhausted", &stream.is_none())
            .field("schema", self.schema.as_ref())
            .finish()
    }
}

impl DisplayAs for OneShotExec {
    fn fmt_as(
        &self,
        t: datafusion::physical_plan::DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        let stream = self.stream.lock().unwrap();
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                let exhausted = if stream.is_some() { "" } else { "EXHAUSTED" };
                let columns = self
                    .schema
                    .field_names()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                write!(
                    f,
                    "OneShotStream: {}columns=[{}]",
                    exhausted,
                    columns.join(",")
                )
            }
        }
    }
}

impl ExecutionPlan for OneShotExec {
    fn name(&self) -> &str {
        "OneShotExec"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema(&self) -> arrow_schema::SchemaRef {
        self.schema.clone()
    }

    fn children(&self) -> Vec<&Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> datafusion_common::Result<Arc<dyn ExecutionPlan>> {
        todo!()
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<datafusion::execution::TaskContext>,
    ) -> datafusion_common::Result<SendableRecordBatchStream> {
        let stream = self
            .stream
            .lock()
            .map_err(|err| DataFusionError::Execution(err.to_string()))?
            .take();
        if let Some(stream) = stream {
            Ok(stream)
        } else {
            Err(DataFusionError::Execution(
                "OneShotExec has already been executed".to_string(),
            ))
        }
    }

    fn statistics(&self) -> datafusion_common::Result<datafusion_common::Statistics> {
        Ok(Statistics::new_unknown(&self.schema))
    }

    fn properties(&self) -> &datafusion::physical_plan::PlanProperties {
        &self.properties
    }
}

/// Callback for reporting statistics after a scan
pub type ExecutionStatsCallback = Arc<dyn Fn(&ExecutionSummaryCounts) + Send + Sync>;

#[derive(Default, Clone)]
pub struct LanceExecutionOptions {
    pub use_spilling: bool,
    pub mem_pool_size: Option<u64>,
    pub batch_size: Option<usize>,
    pub target_partition: Option<usize>,
    pub execution_stats_callback: Option<ExecutionStatsCallback>,
}

impl std::fmt::Debug for LanceExecutionOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LanceExecutionOptions")
            .field("use_spilling", &self.use_spilling)
            .field("mem_pool_size", &self.mem_pool_size)
            .field("batch_size", &self.batch_size)
            .field("target_partition", &self.target_partition)
            .field(
                "execution_stats_callback",
                &self.execution_stats_callback.is_some(),
            )
            .finish()
    }
}

const DEFAULT_LANCE_MEM_POOL_SIZE: u64 = 100 * 1024 * 1024;

impl LanceExecutionOptions {
    pub fn mem_pool_size(&self) -> u64 {
        self.mem_pool_size.unwrap_or_else(|| {
            std::env::var("LANCE_MEM_POOL_SIZE")
                .map(|s| match s.parse::<u64>() {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("Failed to parse LANCE_MEM_POOL_SIZE: {}, using default", e);
                        DEFAULT_LANCE_MEM_POOL_SIZE
                    }
                })
                .unwrap_or(DEFAULT_LANCE_MEM_POOL_SIZE)
        })
    }

    pub fn use_spilling(&self) -> bool {
        if !self.use_spilling {
            return false;
        }
        std::env::var("LANCE_BYPASS_SPILLING")
            .map(|_| {
                info!("Bypassing spilling because LANCE_BYPASS_SPILLING is set");
                false
            })
            .unwrap_or(true)
    }
}

pub fn new_session_context(options: &LanceExecutionOptions) -> SessionContext {
    let mut session_config = SessionConfig::new();
    let mut runtime_env_builder = RuntimeEnvBuilder::new();
    if let Some(target_partition) = options.target_partition {
        session_config = session_config.with_target_partitions(target_partition);
    }
    if options.use_spilling() {
        runtime_env_builder = runtime_env_builder
            .with_disk_manager(DiskManagerConfig::new())
            .with_memory_pool(Arc::new(FairSpillPool::new(
                options.mem_pool_size() as usize
            )));
    }
    let runtime_env = runtime_env_builder.build_arc().unwrap();
    SessionContext::new_with_config_rt(session_config, runtime_env)
}

lazy_static! {
    static ref DEFAULT_SESSION_CONTEXT: SessionContext =
        new_session_context(&LanceExecutionOptions::default());
    static ref DEFAULT_SESSION_CONTEXT_WITH_SPILLING: SessionContext = {
        new_session_context(&LanceExecutionOptions {
            use_spilling: true,
            ..Default::default()
        })
    };
}

pub fn get_session_context(options: &LanceExecutionOptions) -> SessionContext {
    if options.mem_pool_size() == DEFAULT_LANCE_MEM_POOL_SIZE && options.target_partition.is_none()
    {
        return if options.use_spilling() {
            DEFAULT_SESSION_CONTEXT_WITH_SPILLING.clone()
        } else {
            DEFAULT_SESSION_CONTEXT.clone()
        };
    }
    new_session_context(options)
}

fn get_task_context(
    session_ctx: &SessionContext,
    options: &LanceExecutionOptions,
) -> Arc<TaskContext> {
    let mut state = session_ctx.state();
    if let Some(batch_size) = options.batch_size.as_ref() {
        state.config_mut().options_mut().execution.batch_size = *batch_size;
    }

    state.task_ctx()
}

#[derive(Default)]
pub struct ExecutionSummaryCounts {
    pub iops: usize,
    pub requests: usize,
    pub bytes_read: usize,
    pub indices_loaded: usize,
    pub parts_loaded: usize,
    pub index_comparisons: usize,
}

fn visit_node(node: &dyn ExecutionPlan, counts: &mut ExecutionSummaryCounts) {
    if let Some(metrics) = node.metrics() {
        counts.iops += metrics
            .find_count(IOPS_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
        counts.requests += metrics
            .find_count(REQUESTS_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
        counts.bytes_read += metrics
            .find_count(BYTES_READ_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
        counts.indices_loaded += metrics
            .find_count(INDICES_LOADED_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
        counts.parts_loaded += metrics
            .find_count(PARTS_LOADED_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
        counts.index_comparisons += metrics
            .find_count(INDEX_COMPARISONS_METRIC)
            .map(|c| c.value())
            .unwrap_or(0);
    }
    for child in node.children() {
        visit_node(child.as_ref(), counts);
    }
}

fn report_plan_summary_metrics(plan: &dyn ExecutionPlan, options: &LanceExecutionOptions) {
    let output_rows = plan
        .metrics()
        .map(|m| m.output_rows().unwrap_or(0))
        .unwrap_or(0);
    let mut counts = ExecutionSummaryCounts::default();
    visit_node(plan, &mut counts);
    tracing::info!(
        target: TRACE_EXECUTION,
        type = EXECUTION_PLAN_RUN,
        output_rows,
        iops = counts.iops,
        requests = counts.requests,
        bytes_read = counts.bytes_read,
        indices_loaded = counts.indices_loaded,
        parts_loaded = counts.parts_loaded,
        index_comparisons = counts.index_comparisons,
    );
    if let Some(callback) = options.execution_stats_callback.as_ref() {
        callback(&counts);
    }
}

/// Executes a plan using default session & runtime configuration
///
/// Only executes a single partition.  Panics if the plan has more than one partition.
pub fn execute_plan(
    plan: Arc<dyn ExecutionPlan>,
    options: LanceExecutionOptions,
) -> Result<SendableRecordBatchStream> {
    debug!(
        "Executing plan:\n{}",
        DisplayableExecutionPlan::new(plan.as_ref()).indent(true)
    );

    let session_ctx = get_session_context(&options);

    // NOTE: we are only executing the first partition here. Therefore, if
    // the plan has more than one partition, we will be missing data.
    assert_eq!(plan.properties().partitioning.partition_count(), 1);
    let stream = plan.execute(0, get_task_context(&session_ctx, &options))?;

    let schema = stream.schema();
    let stream = stream.finally(move || {
        report_plan_summary_metrics(plan.as_ref(), &options);
    });
    Ok(Box::pin(RecordBatchStreamAdapter::new(schema, stream)))
}

pub async fn analyze_plan(
    plan: Arc<dyn ExecutionPlan>,
    options: LanceExecutionOptions,
) -> Result<String> {
    let schema = plan.schema();
    let analyze = Arc::new(AnalyzeExec::new(true, true, plan, schema));

    let session_ctx = get_session_context(&options);
    assert_eq!(analyze.properties().partitioning.partition_count(), 1);
    let mut stream = analyze
        .execute(0, get_task_context(&session_ctx, &options))
        .map_err(|err| {
            Error::io(
                format!("Failed to execute analyze plan: {}", err),
                location!(),
            )
        })?;

    // fully execute the plan
    while (stream.next().await).is_some() {}

    let display = DisplayableExecutionPlan::with_metrics(analyze.as_ref());
    Ok(format!("{}", display.indent(true)))
}

pub trait SessionContextExt {
    /// Creates a DataFrame for reading a stream of data
    ///
    /// This dataframe may only be queried once, future queries will fail
    fn read_one_shot(
        &self,
        data: SendableRecordBatchStream,
    ) -> datafusion::common::Result<DataFrame>;
}

struct OneShotPartitionStream {
    data: Arc<Mutex<Option<SendableRecordBatchStream>>>,
    schema: Arc<ArrowSchema>,
}

impl std::fmt::Debug for OneShotPartitionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let data = self.data.lock().unwrap();
        f.debug_struct("OneShotPartitionStream")
            .field("exhausted", &data.is_none())
            .field("schema", self.schema.as_ref())
            .finish()
    }
}

impl OneShotPartitionStream {
    fn new(data: SendableRecordBatchStream) -> Self {
        let schema = data.schema();
        Self {
            data: Arc::new(Mutex::new(Some(data))),
            schema,
        }
    }
}

impl PartitionStream for OneShotPartitionStream {
    fn schema(&self) -> &arrow_schema::SchemaRef {
        &self.schema
    }

    fn execute(&self, _ctx: Arc<TaskContext>) -> SendableRecordBatchStream {
        let mut stream = self.data.lock().unwrap();
        stream
            .take()
            .expect("Attempt to consume a one shot dataframe multiple times")
    }
}

impl SessionContextExt for SessionContext {
    fn read_one_shot(
        &self,
        data: SendableRecordBatchStream,
    ) -> datafusion::common::Result<DataFrame> {
        let schema = data.schema();
        let part_stream = Arc::new(OneShotPartitionStream::new(data));
        let provider = StreamingTable::try_new(schema, vec![part_stream])?;
        self.read_table(Arc::new(provider))
    }
}
