use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::mem;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context};
use clap::Parser;
use console::style;
use database_utils::{DatabaseConnection, DatabaseURL};
use itertools::Itertools;
use nom_sql::{
    parse_query, BinaryOperator, CreateTableStatement, DeleteStatement, Dialect, Expr, SqlQuery,
};
use query_generator::{GeneratorState, QuerySeed};

use crate::ast::{Query, QueryParams, QueryResults, Record, SortMode, Statement, StatementResult};
use crate::runner::TestScript;

/// Default value for [`Seed::hash_threshold`]
const DEFAULT_HASH_THRESHOLD: usize = 20;

#[derive(Debug)]
enum Relation {
    Table(String),
    View(String),
}

impl Relation {
    fn kind(&self) -> &'static str {
        match self {
            Relation::Table(_) => "TABLE",
            Relation::View(_) => "VIEW",
        }
    }

    fn name(&self) -> &str {
        match self {
            Relation::Table(name) => name,
            Relation::View(name) => name,
        }
    }
}

#[derive(Debug)]
pub(crate) struct Seed {
    /// Relations to drop (if they exist) before seeding the reference db, to account for having
    /// previously run the test script
    relations_to_drop: Vec<Relation>,
    tables: Vec<CreateTableStatement>,
    queries: Vec<Query>,
    generator: GeneratorState,
    hash_threshold: usize,
    script: TestScript,
}

impl TryFrom<PathBuf> for Seed {
    type Error = anyhow::Error;

    fn try_from(path: PathBuf) -> Result<Self, Self::Error> {
        let mut file = File::open(&path)?;
        let script = TestScript::read(path, &mut file)?;

        let mut relations_to_drop = vec![];
        let mut tables = vec![];
        let mut queries = vec![];
        let mut hash_threshold = DEFAULT_HASH_THRESHOLD;

        for record in script.records() {
            match record {
                Record::Statement(Statement { command, .. }) => {
                    // TODO(grfn): Make dialect configurable
                    match parse_query(Dialect::MySQL, command).map_err(|s| anyhow!("{}", s))? {
                        SqlQuery::CreateTable(tbl) => {
                            relations_to_drop.push(Relation::Table(tbl.table.name.to_string()));
                            tables.push(tbl)
                        }
                        SqlQuery::CreateView(view) => {
                            relations_to_drop
                                .push(Relation::View(view.name.display_unquoted().to_string()));
                        }
                        _ => {}
                    }
                }
                Record::Query(query) => {
                    if !query.params.is_empty() {
                        bail!("Queries with params aren't supported yet");
                    }
                    queries.push(query.clone());
                }
                Record::HashThreshold(ht) => {
                    hash_threshold = *ht;
                }
                Record::Halt { .. } => break,
                Record::Graphviz | Record::Sleep(_) => {}
            }
        }

        let generator = GeneratorState::from(tables.clone());

        file.seek(SeekFrom::Start(0))?;
        Ok(Seed {
            relations_to_drop,
            tables,
            queries,
            generator,
            hash_threshold,
            script,
        })
    }
}

impl TryFrom<query_generator::GenerateOpts> for Seed {
    type Error = anyhow::Error;

    fn try_from(opts: query_generator::GenerateOpts) -> Result<Self, Self::Error> {
        Self::try_from(opts.into_query_seeds().collect::<Vec<_>>())
    }
}

impl TryFrom<Vec<QuerySeed>> for Seed {
    type Error = anyhow::Error;

    fn try_from(seeds: Vec<QuerySeed>) -> Result<Self, Self::Error> {
        let mut generator = query_generator::GeneratorState::default();
        let queries = seeds
            .into_iter()
            .map(|seed| -> anyhow::Result<Query> {
                let query = generator.generate_query(seed);

                // FIXME: Use correct dialect.
                // NOTE: Without a binding, there is a compile error that `statement` does not live
                // long enough if this expression is at `query:`.
                let query_string = query.statement.display(nom_sql::Dialect::MySQL).to_string();

                Ok(Query {
                    label: None,
                    column_types: None,
                    sort_mode: if query.statement.order.is_some() {
                        Some(SortMode::NoSort)
                    } else {
                        Some(SortMode::RowSort)
                    },
                    conditionals: vec![],
                    query: query_string,
                    results: Default::default(),
                    params: QueryParams::PositionalParams(
                        query
                            .state
                            .key()
                            .into_iter()
                            .map(|dt| dt.try_into())
                            .collect::<Result<Vec<_>, _>>()?,
                    ),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut relations_to_drop = vec![];
        let mut tables = vec![];
        let mut records = vec![];

        for (name, table) in generator.tables_mut() {
            table.primary_key(); // ensure the table has a primary key
            let create_stmt = CreateTableStatement::from(table.clone());

            records.push(Record::Statement(Statement {
                result: StatementResult::Ok,
                // FIXME: Use correct dialect.
                command: create_stmt.display(nom_sql::Dialect::MySQL).to_string(),
                conditionals: vec![],
            }));
            tables.push(create_stmt);
            relations_to_drop.push(Relation::Table(name.to_string()));
        }

        Ok(Seed {
            relations_to_drop,
            tables,
            queries,
            generator,
            hash_threshold: DEFAULT_HASH_THRESHOLD,
            script: records.into(),
        })
    }
}

async fn run_queries(
    queries: &[Query],
    conn: &mut DatabaseConnection,
    hash_threshold: usize,
) -> anyhow::Result<Vec<Record>> {
    eprintln!(
        "{}",
        style(format!("==> Running {} queries", queries.len())).bold()
    );

    let mut ret = Vec::new();
    for q in queries {
        let mut results = conn
            .execute(&q.query, q.params.clone())
            .await
            .with_context(|| format!("Running query {}", q.query))?;

        let values: Vec<_> = match q.sort_mode.unwrap_or_default() {
            SortMode::NoSort => results.into_iter().flatten().collect(),
            SortMode::RowSort => {
                results.sort();
                results.into_iter().flatten().collect()
            }
            SortMode::ValueSort => {
                let mut vals: Vec<_> = results.into_iter().flatten().collect();
                vals.sort();
                vals
            }
        };

        let query_results = if values.len() > hash_threshold {
            QueryResults::hash(&values)
        } else {
            QueryResults::Results(values)
        };

        ret.push(Record::Query(Query {
            results: query_results,
            ..q.clone()
        }))
    }

    Ok(ret)
}

impl Seed {
    pub async fn run(&mut self, opts: GenerateOpts) -> anyhow::Result<&TestScript> {
        let mut conn = opts
            .compare_to
            .connect(None)
            .await
            .context("Connecting to comparison database")?;

        eprintln!(
            "{}",
            style(format!(
                "==> Dropping {} relations",
                self.relations_to_drop.len()
            ))
            .bold()
        );
        self.relations_to_drop.reverse();
        for relation in &self.relations_to_drop {
            if opts.verbose {
                eprintln!("    > Dropping {} {}", relation.kind(), relation.name());
            }
            conn.query_drop(format!(
                "DROP {} IF EXISTS {}",
                relation.kind(),
                relation.name()
            ))
            .await
            .with_context(|| format!("Dropping {} {}", relation.kind(), relation.name()))?;
        }

        let tables_in_order = self
            .tables
            .iter()
            .map(|t| t.table.name.clone())
            .collect::<Vec<_>>();

        let data = tables_in_order
            .clone()
            .into_iter()
            .map(|table_name| {
                let spec = self.generator.table_mut(table_name.as_str()).unwrap();
                (
                    table_name,
                    spec.generate_data(opts.rows_per_table, opts.random),
                )
            })
            .collect::<Vec<_>>();

        let insert_statements = data
            .iter()
            .map(|(table_name, data)| {
                let spec = self.generator.table(table_name.as_str()).unwrap();
                let columns = spec.columns.keys().collect::<Vec<_>>();
                nom_sql::InsertStatement {
                    table: spec.name.clone().into(),
                    fields: Some(columns.iter().map(|cn| (*cn).clone().into()).collect()),
                    data: data
                        .clone()
                        .into_iter()
                        .map(|mut row| {
                            columns
                                .iter()
                                .map(|col| {
                                    Expr::Literal(row.remove(col).unwrap().try_into().unwrap())
                                })
                                .collect()
                        })
                        .collect(),
                    ignore: false,
                    on_duplicate: None,
                }
            })
            .collect::<Vec<_>>();

        eprintln!("{}", style("==> Running original test script").bold());
        self.script
            .run_on_database(&Default::default(), &mut conn, None)
            .await?;

        eprintln!(
            "{}",
            style(format!(
                "==> Running {} insert statements",
                insert_statements.len()
            ))
            .bold()
        );
        for insert_statement in &insert_statements {
            if opts.verbose {
                eprintln!(
                    "     > Inserting {} rows of seed data into {}",
                    opts.rows_per_table,
                    insert_statement.table.display_unquoted()
                );
            }
            conn.query_drop(
                insert_statement
                    .display(nom_sql::Dialect::MySQL)
                    .to_string(),
            )
            .await
            .with_context(|| {
                format!(
                    "Inserting seed data for {}",
                    insert_statement.table.display_unquoted()
                )
            })?;
        }

        let new_entries = insert_statements.iter().map(|stmt| {
            // FIXME: Use correct dialect.
            Record::Statement(Statement::ok(
                stmt.display(nom_sql::Dialect::MySQL).to_string(),
            ))
        });

        let hash_threshold = self.hash_threshold;
        let queries = mem::take(&mut self.queries);

        let new_entries =
            new_entries.chain(run_queries(&queries, &mut conn, hash_threshold).await?);

        if opts.include_deletes {
            let rows_to_delete = opts.rows_to_delete.unwrap_or(opts.rows_per_table / 2);

            let delete_statements: Vec<DeleteStatement> = data
                .iter()
                .map(|(table_name, data)| {
                    let spec = self.generator.table(table_name.as_str()).unwrap();
                    let table: nom_sql::Relation = spec.name.clone().into();
                    let pk = spec.primary_key.clone().ok_or_else(|| {
                        anyhow!(
                            "--include-deletes specified, but table {} missing a primary key",
                            table.display_unquoted()
                        )
                    })?;

                    Ok(data
                        .iter()
                        .take(rows_to_delete)
                        .map(|row| DeleteStatement {
                            table: table.clone(),
                            where_clause: Some(Expr::BinaryOp {
                                lhs: Box::new(Expr::Column(pk.clone().into())),
                                op: BinaryOperator::Equal,
                                rhs: Box::new(Expr::Literal(row[&pk].clone().try_into().unwrap())),
                            }),
                        })
                        .collect::<Vec<_>>())
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();

            let new_entries = new_entries.chain(delete_statements.iter().map(|stmt| {
                // FIXME: Use correct dialect.
                Record::Statement(Statement::ok(
                    stmt.display(nom_sql::Dialect::MySQL).to_string(),
                ))
            }));

            eprintln!(
                "{}",
                style(format!(
                    "==> Running {} delete statements",
                    delete_statements.len()
                ))
                .bold()
            );

            for delete_statement in &delete_statements {
                if opts.verbose {
                    eprintln!(
                        "     > Deleting {} rows of seed data from {}",
                        rows_to_delete,
                        delete_statement.table.display_unquoted()
                    );
                }

                conn.query_drop(
                    delete_statement
                        .display(nom_sql::Dialect::MySQL)
                        .to_string(),
                )
                .await
                .with_context(|| {
                    format!(
                        "Deleting seed data for {}",
                        delete_statement.table.display_unquoted()
                    )
                })?;
            }

            self.script
                .extend(new_entries.chain(run_queries(&queries, &mut conn, hash_threshold).await?))
        } else {
            self.script.extend(new_entries)
        }

        Ok(&self.script)
    }
}

// shared options for generating tests
// (not a doc-comment due to https://github.com/clap-rs/clap/issues/2527)
#[derive(Parser, Debug, Clone)]
pub struct GenerateOpts {
    /// URL of a reference database to compare to. Currently supports `mysql://` URLs, but may be
    /// expanded in the future
    #[clap(long)]
    pub compare_to: DatabaseURL,

    /// Rows of data to generate per table
    #[clap(long, default_value = "100")]
    pub rows_per_table: usize,

    /// Enable verbose output
    #[clap(long, short = 'v')]
    pub verbose: bool,

    /// Enable randomly generating column data.
    #[clap(long)]
    pub random: bool,

    /// Whether to include row deletes followed by additional queries in the generated test script.
    ///
    /// If used with a seed script, all tables must have a primary key (due to current limitations
    /// in ReadySet).
    #[clap(long)]
    pub include_deletes: bool,

    /// How many rows to delete in between queries. Ignored if `--include-deletes` is not
    /// specified. Defaults to half of --rows-per-table, rounded down
    #[clap(long)]
    pub rows_to_delete: Option<usize>,
}

/// Generate test scripts by comparing results against a reference database
///
/// The `generate` command takes either a seed script to generate from or a set of [generate
/// options][0], and generates a logictest test script by running queries against a reference
/// database and saving the results
///
/// [0]: GenerateOpts
#[derive(Parser)]
pub struct Generate {
    /// Test script to use as a seed. Seed scripts should contain DDL and queries, but no data.
    pub from: Option<PathBuf>,

    #[clap(flatten)]
    pub query_options: query_generator::GenerateOpts,

    #[clap(flatten)]
    pub script_options: GenerateOpts,

    /// File to write results to (defaults to stdout)
    #[clap(short = 'o')]
    pub output: Option<PathBuf>,
}

fn write_output<W>(script: &TestScript, output: &mut W) -> io::Result<()>
where
    W: io::Write,
{
    writeln!(output, "# Generated by:")?;
    writeln!(output, "#     {}", std::env::args().join(" "))?;

    for rec in script.records() {
        writeln!(output, "{}", rec)?;
    }

    Ok(())
}

impl Generate {
    #[tokio::main]
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut seed = match self.from.take() {
            Some(path) => Seed::try_from(path)?,
            None => Seed::try_from(self.query_options.clone())?,
        };

        let script = seed.run(self.script_options).await?;

        if let Some(out_path) = self.output {
            write_output(
                script,
                &mut File::create(out_path).context("Opening output file")?,
            )?;
        } else {
            write_output(script, &mut io::stdout())?;
        }

        Ok(())
    }
}
