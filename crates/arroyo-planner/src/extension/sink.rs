use std::sync::Arc;

use arroyo_datastream::logical::{LogicalEdge, LogicalEdgeType, LogicalNode, OperatorName};
use arroyo_rpc::{
    df::{ArroyoSchema, ArroyoSchemaRef},
    UPDATING_META_FIELD,
};
use datafusion::common::{internal_err, plan_err, DFSchemaRef, Result, TableReference};

use datafusion::logical_expr::{Expr, Extension, LogicalPlan, UserDefinedLogicalNodeCore};

use prost::Message;

use crate::{
    builder::{NamedNode, Planner},
    multifield_partial_ord,
    tables::Table,
};

use super::{
    debezium::ToDebeziumExtension, remote_table::RemoteTableExtension, ArroyoExtension,
    NodeWithIncomingEdges,
};

pub(crate) const SINK_NODE_NAME: &str = "SinkExtension";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SinkExtension {
    pub(crate) name: TableReference,
    pub(crate) table: Table,
    pub(crate) schema: DFSchemaRef,
    input: Arc<LogicalPlan>,
}

multifield_partial_ord!(SinkExtension, name, input);

impl SinkExtension {
    pub fn new(
        name: TableReference,
        table: Table,
        mut schema: DFSchemaRef,
        mut input: Arc<LogicalPlan>,
    ) -> Result<Self> {
        let input_is_updating = input
            .schema()
            .has_column_with_unqualified_name(UPDATING_META_FIELD);
        match &table {
            Table::ConnectorTable(connector_table) => {
                match (input_is_updating, connector_table.is_updating()) {
                    (_, true) => {
                        let to_debezium_extension =
                            ToDebeziumExtension::try_new(input.as_ref().clone())?;
                        input = Arc::new(LogicalPlan::Extension(Extension {
                            node: Arc::new(to_debezium_extension),
                        }));
                        schema = input.schema().clone();
                    }
                    (true, false) => {
                        return plan_err!("input is updating, but sink is not updating");
                    }
                    (false, false) => {}
                }
            }
            Table::MemoryTable { .. } => return plan_err!("memory tables not supported"),
            Table::TableFromQuery { .. } => {}
            Table::PreviewSink { .. } => {
                if input_is_updating {
                    let to_debezium_extension =
                        ToDebeziumExtension::try_new(input.as_ref().clone())?;
                    input = Arc::new(LogicalPlan::Extension(Extension {
                        node: Arc::new(to_debezium_extension),
                    }));
                    schema = input.schema().clone();
                }
            }
        }
        Self::add_remote_if_necessary(&schema, &mut input);

        Ok(Self {
            name,
            table,
            schema,
            input,
        })
    }

    /* The input to a sink needs to be a non-transparent logical plan extension.
      If it isn't, wrap the input in a RemoteTableExtension.
    */
    pub fn add_remote_if_necessary(schema: &DFSchemaRef, input: &mut Arc<LogicalPlan>) {
        if let LogicalPlan::Extension(node) = input.as_ref() {
            let arroyo_extension: &dyn ArroyoExtension = (&node.node).try_into().unwrap();
            if !arroyo_extension.transparent() {
                return;
            }
        }
        let remote_table_extension = RemoteTableExtension {
            input: input.as_ref().clone(),
            name: TableReference::bare("sink projection"),
            schema: schema.clone(),
            materialize: false,
        };
        *input = Arc::new(LogicalPlan::Extension(Extension {
            node: Arc::new(remote_table_extension),
        }));
    }
}

impl UserDefinedLogicalNodeCore for SinkExtension {
    fn name(&self) -> &str {
        SINK_NODE_NAME
    }

    fn inputs(&self) -> Vec<&LogicalPlan> {
        vec![&self.input]
    }

    fn schema(&self) -> &DFSchemaRef {
        &self.schema
    }

    fn expressions(&self) -> Vec<Expr> {
        vec![]
    }

    fn fmt_for_explain(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "SinkExtension({:?}): {}", self.name, self.schema)
    }

    fn with_exprs_and_inputs(&self, _exprs: Vec<Expr>, inputs: Vec<LogicalPlan>) -> Result<Self> {
        if inputs.len() != 1 {
            return internal_err!("input size inconsistent");
        }

        Ok(Self {
            name: self.name.clone(),
            table: self.table.clone(),
            schema: self.schema.clone(),
            input: Arc::new(inputs[0].clone()),
        })
    }
}

impl ArroyoExtension for SinkExtension {
    fn node_name(&self) -> Option<NamedNode> {
        None
    }

    fn plan_node(
        &self,
        _planner: &Planner,
        index: usize,
        input_schemas: Vec<ArroyoSchemaRef>,
    ) -> Result<NodeWithIncomingEdges> {
        if input_schemas.len() != 1 {
            return plan_err!("sink should have exactly one input");
        }
        // should have exactly one input
        let input_schema = input_schemas[0].clone();

        let operator_config = (self
            .table
            .connector_op()
            .map_err(|e| e.context("connector op"))?)
        .encode_to_vec();
        let node = LogicalNode {
            operator_id: format!("sink_{}_{}", self.name, index),
            description: self.table.connector_op().unwrap().description.clone(),
            operator_name: OperatorName::ConnectorSink,
            parallelism: 1,
            operator_config,
        };
        let edge = LogicalEdge::project_all(LogicalEdgeType::Forward, (*input_schema).clone());
        Ok(NodeWithIncomingEdges {
            node,
            edges: vec![edge],
        })
    }

    fn output_schema(&self) -> ArroyoSchema {
        // this is kinda fake?
        ArroyoSchema::from_schema_keys(Arc::new(self.input.schema().as_ref().into()), vec![])
            .unwrap()
    }
}
