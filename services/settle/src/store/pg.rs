// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's store in PostgreSQL (feature `postgres`; the accepted proposal): a database and
//! a role apart from ingress's (`migrations/0001_settle.sql`).
//!
//! Every connection runs with `synchronous_commit = on`, so a write the adapter acts on (an
//! instruction before its acknowledgement, signed bytes before their broadcast) is flushed before
//! the call returns (`REQ-SA-6`). The store's rules are the database's: one extrinsic per
//! operation reference by primary key, the bytes of a stored extrinsic immutable by trigger, and
//! the lease epoch checked inside the transaction that stores an extrinsic or moves a lane.

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use ingress::config::Profile;
use ledger_core::account::Time;
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{
    AlertRow, AnchorRow, ExtrinsicRow, GapRow, Inserted, InstructionRow, InstructionState, LaneRow,
    Lease, ObservationRow, ReportRow, Store, StoreError, StoreResult,
};

const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("../../migrations/0001_settle.sql")),
    (2, include_str!("../../migrations/0002_anchors.sql")),
    (3, include_str!("../../migrations/0003_batches.sql")),
    (4, include_str!("../../migrations/0004_binds.sql")),
    (5, include_str!("../../migrations/0005_runtimes.sql")),
    (6, include_str!("../../migrations/0006_chain.sql")),
];

/// The adapter's store in PostgreSQL. Clones share the pool.
#[derive(Clone)]
pub struct PgStore {
    pool: Pool,
}

fn err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

fn i64_of(v: u64) -> StoreResult<i64> {
    i64::try_from(v).map_err(|_| StoreError::Backend(format!("{v} does not fit the column")))
}

fn u64_of(v: i64) -> StoreResult<u64> {
    u64::try_from(v).map_err(|_| StoreError::Backend(format!("{v} is negative")))
}

fn to_json<T: Serialize>(t: &T) -> StoreResult<String> {
    serde_json::to_string(t).map_err(err)
}

fn from_json<T: DeserializeOwned>(s: &str) -> StoreResult<T> {
    serde_json::from_str(s).map_err(err)
}

impl PgStore {
    /// Connect to `url` (it may carry a password: never logged) and apply the migrations. TLS as
    /// the connection string's `sslmode` asks, checked as ingress checks it
    /// ([`ingress::store::pg::tls`]). Under [`Profile::Production`] a string that does not
    /// require TLS is refused before any connection is made ([`ingress::store::pg::check_tls`],
    /// `T-016-13`).
    pub async fn connect(url: &str, pool_size: usize, profile: Profile) -> StoreResult<PgStore> {
        let mut config: tokio_postgres::Config = url.parse().map_err(err)?;
        ingress::store::pg::check_tls(&config, profile).map_err(StoreError::Backend)?;
        config.options("-c synchronous_commit=on");
        let manager = Manager::from_config(
            config,
            ingress::store::pg::tls().map_err(StoreError::Backend)?,
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(pool_size.max(1))
            .build()
            .map_err(err)?;
        let store = PgStore { pool };
        store.migrate().await?;
        Ok(store)
    }

    async fn client(&self) -> StoreResult<deadpool_postgres::Object> {
        self.pool.get().await.map_err(err)
    }

    async fn migrate(&self) -> StoreResult<()> {
        let mut c = self.client().await?;
        c.batch_execute(
            "CREATE TABLE IF NOT EXISTS settle_schema (\
                 version INTEGER PRIMARY KEY, applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
        )
        .await
        .map_err(err)?;
        for (version, sql) in MIGRATIONS {
            let tx = c.transaction().await.map_err(err)?;
            tx.execute("LOCK TABLE settle_schema IN EXCLUSIVE MODE", &[])
                .await
                .map_err(err)?;
            let done = tx
                .query_opt("SELECT 1 FROM settle_schema WHERE version = $1", &[version])
                .await
                .map_err(err)?
                .is_some();
            if !done {
                tx.batch_execute(sql).await.map_err(err)?;
                tx.execute(
                    "INSERT INTO settle_schema (version) VALUES ($1)",
                    &[version],
                )
                .await
                .map_err(err)?;
            }
            tx.commit().await.map_err(err)?;
        }
        Ok(())
    }

    async fn bodies<T: DeserializeOwned>(&self, sql: &str) -> StoreResult<Vec<T>> {
        let c = self.client().await?;
        c.query(sql, &[])
            .await
            .map_err(err)?
            .iter()
            .map(|r| from_json(r.try_get::<_, &str>("body").map_err(err)?))
            .collect()
    }
}

async fn fenced(tx: &tokio_postgres::Transaction<'_>, epoch: u64) -> StoreResult<()> {
    let row = tx
        .query_opt(
            "SELECT epoch FROM settle_lease WHERE id = 1 FOR UPDATE",
            &[],
        )
        .await
        .map_err(err)?;
    match row {
        Some(r) if u64_of(r.try_get("epoch").map_err(err)?)? == epoch => Ok(()),
        _ => Err(StoreError::Fenced),
    }
}

impl PgStore {
    /// One of the chain heads kept beside the cursor (`verified`, `accepted`), if set.
    async fn chain_head_column(
        &self,
        instance: u32,
        column: &str,
    ) -> StoreResult<Option<super::AcceptedHead>> {
        let c = self.client().await?;
        let Some(r) = c
            .query_opt(
                &format!("SELECT {column} FROM settle_cursors WHERE instance = $1"),
                &[&i64::from(instance)],
            )
            .await
            .map_err(err)?
        else {
            return Ok(None);
        };
        let body: Option<String> = r.try_get(0).map_err(err)?;
        body.as_deref().map(from_json).transpose()
    }
}

impl Store for PgStore {
    async fn acquire(&self, holder: &str, now: Time, ttl: Time) -> StoreResult<Option<Lease>> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        tx.execute("LOCK TABLE settle_lease IN EXCLUSIVE MODE", &[])
            .await
            .map_err(err)?;
        let current = tx
            .query_opt(
                "SELECT holder, epoch, expires_at FROM settle_lease WHERE id = 1",
                &[],
            )
            .await
            .map_err(err)?;
        let expires = i64_of(now.saturating_add(ttl))?;
        let epoch = match current {
            Some(r) => {
                let h: String = r.try_get("holder").map_err(err)?;
                let e = u64_of(r.try_get("epoch").map_err(err)?)?;
                let x = u64_of(r.try_get("expires_at").map_err(err)?)?;
                if h != holder && x > now {
                    return Ok(None);
                }
                tx.execute(
                    "UPDATE settle_lease SET holder = $1, epoch = $2, expires_at = $3 WHERE id = 1",
                    &[&holder, &i64_of(e + 1)?, &expires],
                )
                .await
                .map_err(err)?;
                e + 1
            }
            None => {
                tx.execute(
                    "INSERT INTO settle_lease (id, holder, epoch, expires_at) VALUES (1, $1, 1, $2)",
                    &[&holder, &expires],
                )
                .await
                .map_err(err)?;
                1
            }
        };
        tx.commit().await.map_err(err)?;
        Ok(Some(Lease {
            holder: holder.to_owned(),
            epoch,
            expires_at: now.saturating_add(ttl),
        }))
    }

    async fn renew(&self, holder: &str, epoch: u64, now: Time, ttl: Time) -> StoreResult<bool> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE settle_lease SET expires_at = $1 WHERE id = 1 AND holder = $2 AND epoch = $3",
                &[&i64_of(now.saturating_add(ttl))?, &holder, &i64_of(epoch)?],
            )
            .await
            .map_err(err)?;
        Ok(n == 1)
    }

    async fn insert_instruction(&self, row: &InstructionRow) -> StoreResult<Inserted> {
        let c = self.client().await?;
        let n = c
            .execute(
                "INSERT INTO settle_instructions (operation, digest, body) VALUES ($1, $2, $3) \
                 ON CONFLICT (operation) DO NOTHING",
                &[&row.operation, &row.digest, &to_json(row)?],
            )
            .await
            .map_err(err)?;
        if n == 1 {
            return Ok(Inserted::New);
        }
        let r = c
            .query_one(
                "SELECT body FROM settle_instructions WHERE operation = $1",
                &[&row.operation],
            )
            .await
            .map_err(err)?;
        let existing: InstructionRow = from_json(r.try_get::<_, &str>("body").map_err(err)?)?;
        Ok(if existing.digest == row.digest {
            Inserted::Same(existing)
        } else {
            Inserted::Conflict(existing)
        })
    }

    async fn put_instruction(&self, row: &InstructionRow) -> StoreResult<()> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE settle_instructions SET body = $3 WHERE operation = $1 AND digest = $2",
                &[&row.operation, &row.digest, &to_json(row)?],
            )
            .await
            .map_err(err)?;
        if n == 1 {
            Ok(())
        } else {
            Err(StoreError::Missing)
        }
    }

    async fn instructions(&self) -> StoreResult<Vec<InstructionRow>> {
        self.bodies("SELECT body FROM settle_instructions ORDER BY operation")
            .await
    }

    async fn store_signed(
        &self,
        epoch: u64,
        extrinsic: &ExtrinsicRow,
        lane: &LaneRow,
        instruction: &InstructionRow,
    ) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        fenced(&tx, epoch).await?;
        let n = tx
            .execute(
                "INSERT INTO settle_extrinsics (operation, lane, nonce, hash, body) \
                 VALUES ($1, $2, $3, $4, $5) ON CONFLICT (operation) DO NOTHING",
                &[
                    &extrinsic.operation,
                    &extrinsic.lane,
                    &i64_of(extrinsic.nonce)?,
                    &hex::encode(extrinsic.extrinsic.hash),
                    &to_json(extrinsic)?,
                ],
            )
            .await
            .map_err(err)?;
        if n != 1 {
            return Err(StoreError::Duplicate);
        }
        tx.execute(
            "INSERT INTO settle_lanes (lane, body) VALUES ($1, $2) \
             ON CONFLICT (lane) DO UPDATE SET body = EXCLUDED.body",
            &[&lane.lane, &to_json(lane)?],
        )
        .await
        .map_err(err)?;
        let mut i = instruction.clone();
        i.state = InstructionState::Signed;
        let n = tx
            .execute(
                "UPDATE settle_instructions SET body = $2 WHERE operation = $1",
                &[&i.operation, &to_json(&i)?],
            )
            .await
            .map_err(err)?;
        if n != 1 {
            return Err(StoreError::Missing);
        }
        tx.commit().await.map_err(err)
    }

    async fn store_batch(
        &self,
        epoch: u64,
        extrinsics: &[ExtrinsicRow],
        lane: &LaneRow,
        instructions: &[InstructionRow],
    ) -> StoreResult<()> {
        if extrinsics.len() != instructions.len()
            || extrinsics
                .iter()
                .zip(instructions)
                .any(|(e, i)| e.operation != i.operation)
        {
            return Err(StoreError::Missing);
        }
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        fenced(&tx, epoch).await?;
        for extrinsic in extrinsics {
            let n = tx
                .execute(
                    "INSERT INTO settle_extrinsics (operation, lane, nonce, hash, body) \
                     VALUES ($1, $2, $3, $4, $5) ON CONFLICT (operation) DO NOTHING",
                    &[
                        &extrinsic.operation,
                        &extrinsic.lane,
                        &i64_of(extrinsic.nonce)?,
                        &hex::encode(extrinsic.extrinsic.hash),
                        &to_json(extrinsic)?,
                    ],
                )
                .await
                .map_err(err)?;
            // Dropping the transaction rolls every row of the batch back.
            if n != 1 {
                return Err(StoreError::Duplicate);
            }
        }
        tx.execute(
            "INSERT INTO settle_lanes (lane, body) VALUES ($1, $2) \
             ON CONFLICT (lane) DO UPDATE SET body = EXCLUDED.body",
            &[&lane.lane, &to_json(lane)?],
        )
        .await
        .map_err(err)?;
        for instruction in instructions {
            let mut i = instruction.clone();
            i.state = InstructionState::Signed;
            let n = tx
                .execute(
                    "UPDATE settle_instructions SET body = $2 WHERE operation = $1",
                    &[&i.operation, &to_json(&i)?],
                )
                .await
                .map_err(err)?;
            if n != 1 {
                return Err(StoreError::Missing);
            }
        }
        tx.commit().await.map_err(err)
    }

    async fn put_extrinsic(&self, row: &ExtrinsicRow) -> StoreResult<()> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE settle_extrinsics SET body = $3 WHERE operation = $1 AND hash = $2",
                &[
                    &row.operation,
                    &hex::encode(row.extrinsic.hash),
                    &to_json(row)?,
                ],
            )
            .await
            .map_err(err)?;
        if n == 1 {
            Ok(())
        } else {
            Err(StoreError::Missing)
        }
    }

    async fn extrinsics(&self) -> StoreResult<Vec<ExtrinsicRow>> {
        self.bodies("SELECT body FROM settle_extrinsics ORDER BY operation")
            .await
    }

    async fn anchors(&self) -> StoreResult<Vec<AnchorRow>> {
        self.bodies("SELECT body FROM settle_anchors ORDER BY number")
            .await
    }

    async fn binds(&self) -> StoreResult<Vec<super::BindRow>> {
        self.bodies("SELECT body FROM settle_binds ORDER BY reference")
            .await
    }

    async fn runtimes(&self) -> StoreResult<Vec<super::RuntimeRow>> {
        self.bodies("SELECT body FROM settle_runtimes ORDER BY runtime")
            .await
    }

    async fn confirm_runtime(&self, row: &super::RuntimeRow) -> StoreResult<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO settle_runtimes (runtime, body) VALUES ($1, $2) \
             ON CONFLICT (runtime) DO NOTHING",
            &[&row.key, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn put_bind(&self, epoch: u64, row: &super::BindRow) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        fenced(&tx, epoch).await?;
        let existing = tx
            .query_opt(
                "SELECT body FROM settle_binds WHERE reference = $1 FOR UPDATE",
                &[&row.reference],
            )
            .await
            .map_err(err)?;
        if let Some(r) = existing {
            let stored: super::BindRow = from_json(r.try_get::<_, &str>("body").map_err(err)?)?;
            if stored.asset != row.asset
                || stored.location != row.location
                || stored.amount != row.amount
            {
                return Err(StoreError::Duplicate);
            }
        }
        tx.execute(
            "INSERT INTO settle_binds (reference, body) VALUES ($1, $2) \
             ON CONFLICT (reference) DO UPDATE SET body = EXCLUDED.body",
            &[&row.reference, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn put_anchor(&self, epoch: u64, row: &AnchorRow) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        fenced(&tx, epoch).await?;
        let number = i64_of(row.number)?;
        let existing = tx
            .query_opt(
                "SELECT commitment FROM settle_anchors WHERE number = $1 FOR UPDATE",
                &[&number],
            )
            .await
            .map_err(err)?;
        if let Some(r) = existing {
            if r.try_get::<_, &str>("commitment").map_err(err)? != row.commitment {
                return Err(StoreError::Duplicate);
            }
        }
        tx.execute(
            "INSERT INTO settle_anchors (number, commitment, body) VALUES ($1, $2, $3)              ON CONFLICT (number) DO UPDATE SET body = EXCLUDED.body",
            &[&number, &row.commitment, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn lane(&self, lane: &str) -> StoreResult<Option<LaneRow>> {
        let c = self.client().await?;
        c.query_opt("SELECT body FROM settle_lanes WHERE lane = $1", &[&lane])
            .await
            .map_err(err)?
            .map(|r| from_json(r.try_get::<_, &str>("body").map_err(err)?))
            .transpose()
    }

    async fn put_lane(&self, epoch: u64, row: &LaneRow) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        fenced(&tx, epoch).await?;
        tx.execute(
            "INSERT INTO settle_lanes (lane, body) VALUES ($1, $2) \
             ON CONFLICT (lane) DO UPDATE SET body = EXCLUDED.body",
            &[&row.lane, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn insert_observation(&self, row: &ObservationRow) -> StoreResult<bool> {
        let c = self.client().await?;
        let key = (
            i64::from(row.instance),
            i64_of(row.seq)?,
            row.digest.clone(),
        );
        let n = c
            .execute(
                "INSERT INTO settle_observations (instance, seq, digest, body) \
                 VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
                &[&key.0, &key.1, &key.2, &to_json(row)?],
            )
            .await
            .map_err(err)?;
        if n == 1 {
            return Ok(true);
        }
        if row.finalized {
            let r = c
                .query_one(
                    "SELECT body FROM settle_observations \
                     WHERE instance = $1 AND seq = $2 AND digest = $3",
                    &[&key.0, &key.1, &key.2],
                )
                .await
                .map_err(err)?;
            let mut existing: ObservationRow =
                from_json(r.try_get::<_, &str>("body").map_err(err)?)?;
            if !existing.finalized {
                existing.finalized = true;
                c.execute(
                    "UPDATE settle_observations SET body = $4 \
                     WHERE instance = $1 AND seq = $2 AND digest = $3",
                    &[&key.0, &key.1, &key.2, &to_json(&existing)?],
                )
                .await
                .map_err(err)?;
            }
        }
        Ok(false)
    }

    async fn observations(&self) -> StoreResult<Vec<ObservationRow>> {
        self.bodies("SELECT body FROM settle_observations ORDER BY instance, seq, digest")
            .await
    }

    async fn put_report(&self, row: &ReportRow) -> StoreResult<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO settle_reports (key, body) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET body = EXCLUDED.body",
            &[&row.key, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn report(&self, key: &str) -> StoreResult<Option<ReportRow>> {
        let c = self.client().await?;
        c.query_opt("SELECT body FROM settle_reports WHERE key = $1", &[&key])
            .await
            .map_err(err)?
            .map(|r| from_json(r.try_get::<_, &str>("body").map_err(err)?))
            .transpose()
    }

    async fn reports(&self) -> StoreResult<Vec<ReportRow>> {
        self.bodies("SELECT body FROM settle_reports ORDER BY key")
            .await
    }

    async fn cursor(&self, instance: u32) -> StoreResult<u64> {
        let c = self.client().await?;
        match c
            .query_opt(
                "SELECT next_seq FROM settle_cursors WHERE instance = $1",
                &[&i64::from(instance)],
            )
            .await
            .map_err(err)?
        {
            Some(r) => u64_of(r.try_get("next_seq").map_err(err)?),
            None => Ok(0),
        }
    }

    async fn advance_cursor(&self, instance: u32, to: u64) -> StoreResult<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO settle_cursors (instance, next_seq) VALUES ($1, $2) \
             ON CONFLICT (instance) DO UPDATE \
             SET next_seq = GREATEST(settle_cursors.next_seq, EXCLUDED.next_seq)",
            &[&i64::from(instance), &i64_of(to)?],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn accepted_head(&self, instance: u32) -> StoreResult<Option<super::AcceptedHead>> {
        self.chain_head_column(instance, "accepted").await
    }

    async fn verified_head(&self, instance: u32) -> StoreResult<Option<super::AcceptedHead>> {
        self.chain_head_column(instance, "verified").await
    }

    async fn verify_head(
        &self,
        instance: u32,
        head: &super::AcceptedHead,
        entries: &[(u64, Vec<u8>)],
    ) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        for (seq, raw) in entries {
            tx.execute(
                "INSERT INTO settle_chain_entries (instance, seq, raw) VALUES ($1, $2, $3) \
                 ON CONFLICT (instance, seq) DO UPDATE SET raw = EXCLUDED.raw",
                &[&i64::from(instance), &i64_of(*seq)?, &hex::encode(raw)],
            )
            .await
            .map_err(err)?;
        }
        tx.execute(
            "INSERT INTO settle_cursors (instance, next_seq, verified) VALUES ($1, 0, $2) \
             ON CONFLICT (instance) DO UPDATE SET verified = EXCLUDED.verified",
            &[&i64::from(instance), &to_json(head)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn chain_entries(
        &self,
        instance: u32,
        from: u64,
        to: u64,
    ) -> StoreResult<Vec<(u64, Vec<u8>)>> {
        let c = self.client().await?;
        c.query(
            "SELECT seq, raw FROM settle_chain_entries \
             WHERE instance = $1 AND seq >= $2 AND seq < $3 ORDER BY seq",
            &[&i64::from(instance), &i64_of(from)?, &i64_of(to)?],
        )
        .await
        .map_err(err)?
        .into_iter()
        .map(|r| {
            let seq = u64_of(r.try_get("seq").map_err(err)?)?;
            let raw: &str = r.try_get("raw").map_err(err)?;
            Ok((seq, hex::decode(raw).map_err(err)?))
        })
        .collect()
    }

    async fn accept_head(&self, instance: u32, head: &super::AcceptedHead) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        tx.execute(
            "INSERT INTO settle_cursors (instance, next_seq, accepted) VALUES ($1, $2, $3) \
             ON CONFLICT (instance) DO UPDATE SET accepted = EXCLUDED.accepted",
            &[
                &i64::from(instance),
                &i64_of(head.position)?,
                &to_json(head)?,
            ],
        )
        .await
        .map_err(err)?;
        tx.execute(
            "DELETE FROM settle_chain_entries WHERE instance = $1 AND seq < $2",
            &[&i64::from(instance), &i64_of(head.position)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn reset_chain(&self, instance: u32, start: u64) -> StoreResult<()> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        tx.execute(
            "INSERT INTO settle_cursors (instance, next_seq) VALUES ($1, $2) \
             ON CONFLICT (instance) DO UPDATE \
             SET next_seq = EXCLUDED.next_seq, verified = NULL, accepted = NULL",
            &[&i64::from(instance), &i64_of(start)?],
        )
        .await
        .map_err(err)?;
        tx.execute(
            "DELETE FROM settle_chain_entries WHERE instance = $1",
            &[&i64::from(instance)],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn insert_gap(&self, row: &GapRow) -> StoreResult<()> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO settle_gaps (instance, from_seq, body) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
            &[&i64::from(row.instance), &i64_of(row.from)?, &to_json(row)?],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn gaps(&self) -> StoreResult<Vec<GapRow>> {
        self.bodies("SELECT body FROM settle_gaps ORDER BY instance, from_seq")
            .await
    }

    async fn raise(&self, key: &str, detail: &str, now: Time) -> StoreResult<bool> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        let existing = tx
            .query_opt(
                "SELECT body FROM settle_alerts WHERE key = $1 FOR UPDATE",
                &[&key],
            )
            .await
            .map_err(err)?
            .map(|r| from_json::<AlertRow>(r.try_get::<_, &str>("body").unwrap_or("")))
            .transpose()?;
        let (row, new) = match existing {
            Some(a) if a.cleared_at.is_none() => return Ok(false),
            Some(mut a) => {
                a.cleared_at = None;
                a.opened_at = now;
                a.detail = detail.to_owned();
                a.raised += 1;
                (a, true)
            }
            None => (
                AlertRow {
                    key: key.to_owned(),
                    detail: detail.to_owned(),
                    opened_at: now,
                    cleared_at: None,
                    raised: 1,
                },
                true,
            ),
        };
        tx.execute(
            "INSERT INTO settle_alerts (key, body) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET body = EXCLUDED.body",
            &[&key, &to_json(&row)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(new)
    }

    async fn clear(&self, key: &str, now: Time) -> StoreResult<bool> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        let Some(r) = tx
            .query_opt(
                "SELECT body FROM settle_alerts WHERE key = $1 FOR UPDATE",
                &[&key],
            )
            .await
            .map_err(err)?
        else {
            return Ok(false);
        };
        let mut a: AlertRow = from_json(r.try_get::<_, &str>("body").map_err(err)?)?;
        if a.cleared_at.is_some() {
            return Ok(false);
        }
        a.cleared_at = Some(now);
        tx.execute(
            "UPDATE settle_alerts SET body = $2 WHERE key = $1",
            &[&key, &to_json(&a)?],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)?;
        Ok(true)
    }

    async fn alerts(&self) -> StoreResult<Vec<AlertRow>> {
        self.bodies("SELECT body FROM settle_alerts ORDER BY key")
            .await
    }
}
