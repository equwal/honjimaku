use std::{
    borrow::Cow,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use crossbeam_channel::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, trace, warn};

/// A trait that all table-like types must meet.
pub trait Table: Sized {
    /// The table name
    const NAME: &'static str;
    /// An array of all the columns of the table.
    const COLUMNS: &'static [&'static str];
    /// The type of the table's ID column
    type Id;

    /// Conversion from a rusqlite Row to the target type.
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self>;

    /// Creates an update query with the given column names.
    ///
    /// The last parameter is the primary key of the row.
    ///
    /// Panics if the column is not in the COLUMNS array.
    fn update_query(columns: impl AsRef<[&'static str]>) -> String {
        let mut query = format!("UPDATE {} SET ", Self::NAME);
        let mut first = true;
        for column in columns.as_ref() {
            if first {
                first = false;
            } else {
                query.push_str(", ");
            }
            if Self::COLUMNS.contains(column) {
                query.push_str(column);
                query.push_str(" = ?");
            } else {
                panic!("Column {column} is not in the COLUMNS array");
            }
        }
        query.push_str(" WHERE id = ?");
        query
    }
}

type SqliteCall = Box<dyn FnOnce(&mut rusqlite::Connection) + Send + 'static>;
type InitFn = Arc<dyn Fn(&mut rusqlite::Connection) -> rusqlite::Result<()> + Send + Sync + 'static>;

const UNREACHABLE: &str = "connection communication channels unexpectedly terminated";

enum Message {
    Call(SqliteCall),
    Terminate,
}

/// Builder to help open a database connection.
///
/// This is retrieved from [`Datebase::file`].
pub struct DatabaseBuilder {
    path: PathBuf,
    max_connections: usize,
    init: Option<InitFn>,
}

impl DatabaseBuilder {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            max_connections: 10,
            init: None,
        }
    }

    /// Configure how many connections to open.
    ///
    /// These connections are each a separate thread.
    pub fn connections(mut self, max_connections: usize) -> Self {
        self.max_connections = max_connections;
        self
    }

    /// Configure the function to call when the connection is successfully opened.
    ///
    /// Useful for setting certain attributes such as PRAGMAs.
    ///
    /// # Example
    ///
    /// Make a `Database` that sets the `foreign_keys` pragma to
    /// true for every connection.
    ///
    /// ```rust,no_run
    /// let db = Database::file("app.db")
    ///     .with_init(|c| c.execute_batch("PRAGMA foreign_keys=1;"))
    ///     .open()
    ///     .await?;
    /// ```
    pub fn with_init<F>(mut self, init: F) -> Self
    where
        F: Fn(&mut rusqlite::Connection) -> rusqlite::Result<()> + Send + Sync + 'static,
    {
        self.init = Some(Arc::new(init));
        self
    }

    /// Opens the database and the background threads needed for the connection pooling mechanism.
    ///
    /// If any of the connections fail to open then the failure is returned.
    pub async fn open(self) -> rusqlite::Result<Database> {
        let (result_sender, mut result_receiver) = mpsc::channel(self.max_connections);
        let (sender, receiver) = crossbeam_channel::unbounded();

        debug!(
            "creating a threaded connection pool with {} maximum connections to {}",
            self.max_connections,
            self.path.display()
        );
        let mut workers = Vec::with_capacity(self.max_connections);
        for i in 0..self.max_connections {
            workers.push(Worker::new(
                i,
                self.path.clone(),
                result_sender.clone(),
                self.init.clone(),
                receiver.clone(),
            ));
        }

        // Wait for all the results to come through
        drop(result_sender);

        let db = Database { sender, workers };

        // If None is returned then all other senders have been closed
        // Senders close when they have reported either a success or an error
        // when opening a SQLite connection
        while let Some(result) = result_receiver.recv().await {
            if let Err(e) = result {
                debug!("received error while waiting for connection pool to initialise");
                return Err(e);
            }
        }

        Ok(db)
    }
}

/// The handle responsible for all database related queries.
///
/// This manages a few background threads in order to implement proper
/// connection pooling. Each thread maintains one SQLite connection.
pub struct Database {
    sender: Sender<Message>,
    workers: Vec<Worker>,
}

impl Database {
    /// Returns a builder to open the database at the specified file location.
    ///
    /// The :memory: path can be used to denote an in-memory database.
    pub fn file<P: AsRef<Path>>(path: P) -> DatabaseBuilder {
        DatabaseBuilder::new(path.as_ref().to_owned())
    }

    /// Call a function in a background thread with a connection and get the result asynchronously.
    pub async fn call<F, R>(&self, func: F) -> R
    where
        F: FnOnce(&mut rusqlite::Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();

        self.sender
            .send(Message::Call(Box::new(move |conn| {
                let _ = sender.send(func(conn));
            })))
            .expect(UNREACHABLE);

        receiver.await.expect(UNREACHABLE)
    }

    /// Execute the given query with the given parameters with a connection from the pool.
    pub async fn execute<Q, P>(&self, query: Q, params: P) -> rusqlite::Result<usize>
    where
        Q: Into<Cow<'static, str>> + Send,
        P: rusqlite::Params + Send + 'static,
    {
        let query = query.into();
        self.call(move |conn| conn.execute(query.as_ref(), params)).await
    }

    /// Execute the given query with a connection from the pool.
    pub async fn execute_batch<Q>(&self, query: Q) -> rusqlite::Result<()>
    where
        Q: Into<Cow<'static, str>> + Send,
    {
        let query = query.into();
        self.call(move |conn| conn.execute_batch(query.as_ref())).await
    }

    /// Execute the query with the given parameters and get the first result, if any.
    ///
    /// This converts the row to the specified type. If no row is found then `None` is returned.
    pub async fn get<T, Q, P>(&self, query: Q, params: P) -> rusqlite::Result<Option<T>>
    where
        T: Table + Send + 'static,
        P: rusqlite::Params + Send + 'static,
        Q: Into<Cow<'static, str>> + Send,
    {
        let query = query.into();
        self.call(move |conn| -> rusqlite::Result<Option<T>> {
            let mut stmt = conn.prepare_cached(query.as_ref())?;
            match stmt.query_row(params, T::from_row) {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// Execute the query with the given parameters and get the first result, if any.
    ///
    /// This converts the row to the specified type. If no row is found then `None` is returned.
    pub async fn get_row<F, R, Q, P>(&self, query: Q, params: P, func: F) -> rusqlite::Result<R>
    where
        F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
        P: rusqlite::Params + Send + 'static,
        Q: Into<Cow<'static, str>> + Send,
    {
        let query = query.into();
        self.call(move |conn| -> rusqlite::Result<R> {
            let mut stmt = conn.prepare_cached(query.as_ref())?;
            stmt.query_row(params, func)
        })
        .await
    }

    /// Gets a row from its ID.
    pub async fn get_by_id<T>(&self, id: T::Id) -> rusqlite::Result<Option<T>>
    where
        T: Table + Send + 'static,
        T::Id: rusqlite::ToSql + Send + 'static,
    {
        self.call(move |conn| -> rusqlite::Result<Option<T>> {
            let query = format!("SELECT * FROM {} WHERE id=?", T::NAME);
            let mut stmt = conn.prepare_cached(&query)?;
            match stmt.query_row(rusqlite::params![id], T::from_row) {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// Execute the query with the given parameters and returns all results.
    ///
    /// This converts the row to the specified type.
    pub async fn all<T, Q, P>(&self, query: Q, params: P) -> rusqlite::Result<Vec<T>>
    where
        T: Table + Send + 'static,
        P: rusqlite::Params + Send + 'static,
        Q: Into<Cow<'static, str>> + Send,
    {
        let query = query.into();
        self.call(move |conn| -> rusqlite::Result<Vec<T>> {
            let mut stmt = conn.prepare_cached(query.as_ref())?;

            match stmt.query_map(params, T::from_row) {
                Ok(value) => value.collect(),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Vec::new()),
                Err(e) => Err(e),
            }
        })
        .await
    }

    /// Executes the given function within a transaction.
    pub async fn transaction<F, R>(&self, func: F) -> rusqlite::Result<R>
    where
        F: FnOnce(Transaction) -> rusqlite::Result<R> + Send + Sync + 'static,
        R: Send + 'static,
    {
        self.call(move |conn| -> rusqlite::Result<R> {
            let tx = Transaction {
                inner: conn.transaction()?,
            };
            func(tx)
        })
        .await
    }

    /// Gets the value from the key-value store. Returns `None` if not found.
    ///
    /// Unlike other functions here, all errors are coerced into `None` for usability here.
    pub async fn get_from_storage<T>(&self, key: &'static str) -> Option<T>
    where
        T: rusqlite::types::FromSql + Send + 'static,
    {
        self.call(move |conn| -> rusqlite::Result<Option<T>> {
            let query = "SELECT value FROM storage WHERE name = ?";
            let mut stmt = conn.prepare_cached(query)?;
            let result = stmt.query_row([key], |row| row.get(0));
            match result {
                Ok(value) => Ok(Some(value)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e),
            }
        })
        .await
        .ok()
        .flatten()
    }

    /// Updates the value in the key-value store.
    pub async fn update_storage<T>(&self, key: &'static str, value: T) -> rusqlite::Result<()>
    where
        T: rusqlite::types::ToSql + Send + 'static,
    {
        self.call(move |conn| {
            let query = "INSERT INTO storage(name, value) VALUES (?, ?) ON CONFLICT (name) DO UPDATE SET value = excluded.value";
            let mut stmt = conn.prepare_cached(query)?;
            stmt.execute((key, value))?;
            Ok(())
        })
        .await
    }

    /// Removes a value in the key-value store.
    pub async fn remove_from_storage(&self, key: &'static str) -> rusqlite::Result<()> {
        self.call(move |conn| {
            let query = "DELETE FROM storage WHERE name = ?";
            let mut stmt = conn.prepare_cached(query)?;
            stmt.execute((key,))?;
            Ok(())
        })
        .await
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        trace!("sending terminate message to all database connections");

        for worker in &self.workers {
            if !worker.is_finished() {
                // Ignore error because the only error happens if both sides are disconnected
                let _ = self.sender.send(Message::Terminate);
            }
        }

        debug!("shutting down all database connections...");
        for worker in &mut self.workers {
            trace!("terminating database connection worker {}", worker.id);
            worker.terminate();
        }
    }
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database").field("workers", &self.workers).finish()
    }
}

struct Worker {
    id: usize,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    fn new(
        id: usize,
        path: PathBuf,
        result_sender: mpsc::Sender<rusqlite::Result<()>>,
        init: Option<InitFn>,
        receiver: Receiver<Message>,
    ) -> Self {
        let thread = thread::spawn(move || {
            let mut connection = match rusqlite::Connection::open(path) {
                Ok(c) => c,
                Err(e) => {
                    trace!("database connection worker {} received an error ({})", id, &e);
                    let _ = result_sender.blocking_send(Err(e));
                    return;
                }
            };

            trace!("database connection worker {} created connection", id);

            if let Some(f) = init {
                if let Err(e) = f(&mut connection) {
                    trace!(
                        "database connection worker {} received an error ({}) during init",
                        id, &e
                    );
                    let _ = result_sender.blocking_send(Err(e));
                    return;
                }

                trace!("database connection worker {} initialised connection", id);
            }

            trace!(
                "database connection worker {} is signaling to main thread completion",
                id
            );
            if result_sender.blocking_send(Ok(())).is_err() {
                trace!("database connection worker {} received an error during sending", id);
                return;
            }

            drop(result_sender);

            trace!("database connection worker {} has signaled completion", id);

            while let Ok(msg) = receiver.recv() {
                match msg {
                    Message::Call(func) => {
                        trace!("database connection worker {} received request to process call", id);
                        func(&mut connection)
                    }
                    Message::Terminate => break,
                }
            }
        });

        Worker {
            id,
            thread: Some(thread),
        }
    }

    fn terminate(&mut self) {
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!(
                "connection pool worker {} has panicked while cleaning up, ignoring.",
                self.id
            );
        }
    }

    fn is_finished(&self) -> bool {
        match &self.thread {
            Some(thread) => thread.is_finished(),
            None => true,
        }
    }
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// A macro to generate parameters suitable for sending to a connection's thread.
///
/// This causes one allocation per parameter. This is due to a limitation in Rust's
/// type system not allowing variadic generic arguments, compounded by the fact that
/// rusqlite does not support variadic tuples.
#[macro_export]
macro_rules! boxed_params {
    () => {
        [] as [Box<dyn rusqlite::ToSql>; 0]
    };
    ($($param:expr),+ $(,)?) => {
        rusqlite::params_from_iter([$(Box::new($param) as Box<dyn rusqlite::ToSql + Send>),+])
    };
}

pub use boxed_params;

/// An opaque handle to a transaction.
///
/// This automatically dereferences to the inner transaction type.
pub struct Transaction<'conn> {
    inner: rusqlite::Transaction<'conn>,
}

impl<'conn> std::ops::Deref for Transaction<'conn> {
    type Target = rusqlite::Transaction<'conn>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'conn> std::ops::DerefMut for Transaction<'conn> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'conn> Transaction<'conn> {
    /// Execute the query with the given parameters and get the first result, if any.
    ///
    /// This converts the row to the specified type. If no row is found then `None` is returned.
    pub fn get<T, P>(&self, query: &str, params: P) -> rusqlite::Result<Option<T>>
    where
        T: Table,
        P: rusqlite::Params,
    {
        let mut stmt = self.inner.prepare_cached(query)?;
        match stmt.query_row(params, T::from_row) {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Execute the query with the given parameters and returns all results.
    ///
    /// This converts the row to the specified type. If no row is found then `None` is returned.
    pub fn all<T, P>(&self, query: &str, params: P) -> rusqlite::Result<Vec<T>>
    where
        T: Table,
        P: rusqlite::Params,
    {
        let mut stmt = self.inner.prepare_cached(query)?;

        match stmt.query_map(params, T::from_row) {
            Ok(value) => value.collect(),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }
}

/// The changes to the schema, in order. `init` runs the ones that the database does not have yet.
pub const MIGRATIONS: [&str; 8] = [
    include_str!("../sql/0.sql"),
    include_str!("../sql/1.sql"),
    include_str!("../sql/2.sql"),
    include_str!("../sql/3.sql"),
    include_str!("../sql/4.sql"),
    include_str!("../sql/5.sql"),
    include_str!("../sql/6.sql"),
    include_str!("../sql/7.sql"),
];

/// Makes a connection ready for the server: loads the modules, sets the pragmas, and runs
/// the migrations that the database does not have yet.
pub fn init(connection: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    rusqlite::vtab::array::load_module(connection)?;
    // sql/7.sql makes the table directory_entry again. With the foreign keys on, its DROP
    // TABLE would delete the rows of the other tables that point at the old table (the
    // bookmarks, the notifications). So they are off until the migrations are done, then
    // checked, as https://sqlite.org/lang_altertable.html#otheralter says. A pragma does
    // not change inside a transaction, so this is before it.
    connection.execute_batch("PRAGMA foreign_keys=0;\nPRAGMA journal_mode=wal;")?;
    // Each worker of the pool runs this at the same time. When a migration is due, the
    // first worker holds the write lock; the others wait for it, then read the new
    // version and skip. Without the wait the server fails with "database is locked".
    connection.busy_timeout(std::time::Duration::from_secs(60))?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: usize = tx.query_row("PRAGMA user_version;", [], |r| r.get(0))?;
    for migration in MIGRATIONS.iter().skip(version) {
        tx.execute_batch(migration)?;
    }
    tx.commit()?;
    if version < MIGRATIONS.len() {
        let broken: i64 = connection.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
        if broken > 0 {
            warn!("{broken} rows point at a row that is not there after the migrations");
        }
    }
    connection.execute_batch("PRAGMA foreign_keys=1;")
}

/// Checks whether an error is a unique constraint violation.
pub fn is_unique_constraint_violation(e: &rusqlite::Error) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(error, _) => error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE,
        _ => false,
    }
}

/// Returns (and creates) the directory for the main.db file
pub fn directory() -> anyhow::Result<PathBuf> {
    use anyhow::Context;

    let mut path = crate::utils::dir_or_home(dirs::data_dir(), "data")
        .context("could not find a data directory for the current user")?;
    path.push(crate::PROGRAM_NAME);
    if let Err(e) = std::fs::create_dir(&path)
        && e.kind() != std::io::ErrorKind::AlreadyExists
    {
        return Err(e).context("could not create application local data directory");
    }
    path.push("main.db");
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Eq, PartialEq, PartialOrd, Ord)]
    struct Foo {
        id: i64,
        name: String,
        age: i64,
    }

    impl Table for Foo {
        const NAME: &'static str = "foo";
        const COLUMNS: &'static [&'static str] = &["id", "name", "age"];
        type Id = i64;

        fn from_row(row: &rusqlite::Row) -> rusqlite::Result<Self> {
            Ok(Foo {
                id: row.get("id")?,
                name: row.get("name")?,
                age: row.get("age")?,
            })
        }
    }

    #[tokio::test]
    async fn test_basic_connection() {
        let conn = Database::file(":memory:")
            .with_init(|con| {
                con.execute_batch(
                    "CREATE TABLE IF NOT EXISTS foo(id INTEGER PRIMARY KEY, name TEXT, age INTEGER);
            INSERT INTO foo(name, age) VALUES ('bob', 20), ('tanya', 25), ('phil', 25);",
                )
            })
            .open()
            .await
            .expect("could not connect DB");

        conn.execute("INSERT INTO foo(name, age) VALUES (?, ?)", boxed_params!("someone", 13))
            .await
            .expect("execute failed to run");
        let foo: Option<Foo> = conn
            .get("SELECT * FROM foo WHERE id=?", boxed_params!(1))
            .await
            .expect("get failed to run");

        assert!(foo.is_some());
        assert_eq!(
            foo,
            Some(Foo {
                id: 1,
                name: "bob".to_owned(),
                age: 20
            })
        );
    }

    #[test]
    fn test_update_query_creation() {
        let query = Foo::update_query(["name", "age"]);
        assert_eq!(query, "UPDATE foo SET name = ?, age = ? WHERE id = ?");
    }

    /// sql/7.sql makes the table directory_entry again. The rows of the other tables that
    /// point at an entry must stay: bookmarks, notifications, reports, the audit log.
    #[test]
    fn the_rows_that_point_at_an_entry_stay_when_the_table_is_made_again() {
        let path = std::env::temp_dir().join(format!("honjimaku-migration-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA foreign_keys=1;").unwrap();
        for migration in &MIGRATIONS[..7] {
            connection.execute_batch(migration).unwrap();
        }
        connection
            .execute_batch(
                "INSERT INTO account(id, name, password) VALUES (1, 'reader', 'x');
                 INSERT INTO directory_entry(id, path, name, anilist_id) VALUES (5, '/s/a', 'A', 19);
                 INSERT INTO directory_entry(id, path, name, book_id) VALUES (6, '/s/b', 'B', 'B0BPXSSWVF');
                 INSERT INTO bookmark(user_id, entry_id) VALUES (1, 5);
                 INSERT INTO notification(ts, entry_id, user_id, payload) VALUES (1, 5, 1, '{}');
                 INSERT INTO report(id, account_id, entry_id, reason) VALUES (1, 1, 5, 'why');
                 INSERT INTO audit_log(id, entry_id, account_id, data) VALUES (1, 5, 1, '{}');",
            )
            .unwrap();

        init(&mut connection).unwrap();

        let one = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(one("PRAGMA user_version"), MIGRATIONS.len() as i64);
        assert_eq!(one("PRAGMA foreign_keys"), 1, "the foreign keys are on again");
        assert_eq!(one("SELECT count(*) FROM directory_entry"), 2);
        assert_eq!(one("SELECT count(*) FROM bookmark WHERE entry_id = 5"), 1);
        assert_eq!(one("SELECT count(*) FROM notification WHERE entry_id = 5"), 1);
        assert_eq!(one("SELECT count(*) FROM report WHERE entry_id = 5"), 1);
        assert_eq!(one("SELECT count(*) FROM audit_log WHERE entry_id = 5"), 1);
        assert_eq!(one("SELECT count(*) FROM pragma_foreign_key_check"), 0);

        // One show can be here once in each language.
        let add = |path: &str, language: Option<&str>| {
            connection.execute(
                "INSERT INTO directory_entry(path, name, anilist_id, language, kind) VALUES (?, 'A', 19, ?, 'anime')",
                (path, language),
            )
        };
        add("/s/a-zh", Some("zh")).unwrap();
        assert!(add("/s/a-zh-again", Some("zh")).is_err());
        assert!(add("/s/a-again", None).is_err(), "no language is one language too");
        // One audiobook is one entry, in any language.
        let book = connection.execute(
            "INSERT INTO directory_entry(path, name, book_id, language) VALUES ('/s/b-en', 'B', 'B0BPXSSWVF', 'en')",
            [],
        );
        assert!(book.is_err());
        // The mirror table points at an entry, and goes with it.
        connection
            .execute_batch(
                "INSERT INTO mirror(site, remote_id, entry_id) VALUES ('jimaku.cc', 1, 5);
                 DELETE FROM directory_entry WHERE id = 5;",
            )
            .unwrap();
        let one = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).unwrap() };
        assert_eq!(one("SELECT count(*) FROM mirror"), 0);
        assert_eq!(one("SELECT count(*) FROM bookmark"), 0);

        drop(connection);
        std::fs::remove_file(&path).unwrap();
    }
}
