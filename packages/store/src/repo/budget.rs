//! Budgets and the approval ledger.
//!
//! Nothing here moves money. `simulated` is written as `1` on every row and
//! there is no code path that writes anything else.

use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension, Row};

use otwono_types::budget::{
    required_state_for, summarise, Budget, BudgetSummary, Expense, ExpenseState,
};

use crate::Db;

const BUDGET_COLUMNS: &str =
    "id, project_id, name, currency, total_minor, approval_threshold_minor, simulated, created_at";
const EXPENSE_COLUMNS: &str = "id, budget_id, task_id, category, description, amount_minor, \
    state, receipt_path, approved_by, approved_at, simulated, created_at";

fn map_budget(row: &Row<'_>) -> rusqlite::Result<Budget> {
    Ok(Budget {
        id: row.get(0)?,
        project_id: row.get(1)?,
        name: row.get(2)?,
        currency: row.get(3)?,
        total_minor: row.get(4)?,
        approval_threshold_minor: row.get(5)?,
        simulated: row.get::<_, i64>(6)? != 0,
        created_at: crate::parse_ts(&row.get::<_, String>(7)?),
    })
}

fn map_expense(row: &Row<'_>) -> rusqlite::Result<Expense> {
    Ok(Expense {
        id: row.get(0)?,
        budget_id: row.get(1)?,
        task_id: row.get(2)?,
        category: row.get(3)?,
        description: row.get(4)?,
        amount_minor: row.get(5)?,
        state: ExpenseState::parse(&row.get::<_, String>(6)?).unwrap_or(ExpenseState::Estimated),
        receipt_path: row.get(7)?,
        approved_by: row.get(8)?,
        approved_at: crate::parse_ts_opt(row.get(9)?),
        simulated: row.get::<_, i64>(10)? != 0,
        created_at: crate::parse_ts(&row.get::<_, String>(11)?),
    })
}

pub struct BudgetRepo<'a> {
    db: &'a Db,
}

impl<'a> BudgetRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    pub fn create(
        &self,
        project_id: Option<&str>,
        name: &str,
        currency: &str,
        total_minor: i64,
        approval_threshold_minor: i64,
    ) -> Result<Budget> {
        if total_minor < 0 || approval_threshold_minor < 0 {
            bail!("budget amounts must not be negative");
        }
        if currency.len() != 3 || !currency.chars().all(|c| c.is_ascii_alphabetic()) {
            bail!("currency must be a three-letter code, for example USD");
        }
        let id = otwono_types::new_id("bud");
        self.db.conn()?.execute(
            "INSERT INTO budgets
               (id, project_id, name, currency, total_minor, approval_threshold_minor, simulated, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
            params![
                id, project_id, name, currency.to_ascii_uppercase(), total_minor,
                approval_threshold_minor, crate::now_str()
            ],
        )?;
        if let Some(project_id) = project_id {
            self.db.conn()?.execute(
                "UPDATE projects SET budget_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![project_id, id, crate::now_str()],
            )?;
        }
        self.get(&id)?
            .ok_or_else(|| anyhow::anyhow!("budget not found after creation"))
    }

    pub fn get(&self, id: &str) -> Result<Option<Budget>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {BUDGET_COLUMNS} FROM budgets WHERE id = ?1"),
                [id],
                map_budget,
            )
            .optional()?)
    }

    pub fn list(&self) -> Result<Vec<Budget>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {BUDGET_COLUMNS} FROM budgets ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], map_budget)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Record an expense. The starting state is decided by the budget's own
    /// approval threshold, not by the caller — an agent cannot mark its own
    /// spending pre-approved.
    pub fn record_expense(
        &self,
        budget_id: &str,
        task_id: Option<&str>,
        category: &str,
        description: &str,
        amount_minor: i64,
    ) -> Result<Expense> {
        let budget = self
            .get(budget_id)?
            .ok_or_else(|| anyhow::anyhow!("budget {budget_id} does not exist"))?;
        let state = required_state_for(amount_minor, &budget)?;

        let id = otwono_types::new_id("exp");
        self.db.conn()?.execute(
            "INSERT INTO expenses
               (id, budget_id, task_id, category, description, amount_minor, state, simulated, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8)",
            params![
                id, budget_id, task_id, category, description, amount_minor,
                state.as_str(), crate::now_str()
            ],
        )?;
        self.get_expense(&id)?
            .ok_or_else(|| anyhow::anyhow!("expense not found after creation"))
    }

    pub fn get_expense(&self, id: &str) -> Result<Option<Expense>> {
        let conn = self.db.conn()?;
        Ok(conn
            .query_row(
                &format!("SELECT {EXPENSE_COLUMNS} FROM expenses WHERE id = ?1"),
                [id],
                map_expense,
            )
            .optional()?)
    }

    pub fn expenses(&self, budget_id: &str) -> Result<Vec<Expense>> {
        let conn = self.db.conn()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {EXPENSE_COLUMNS} FROM expenses WHERE budget_id = ?1 ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([budget_id], map_expense)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Approve an expense. Refused when it would take the budget over its
    /// total; the user must raise the budget deliberately instead.
    pub fn approve_expense(&self, expense_id: &str, approved_by: &str) -> Result<Expense> {
        let expense = self
            .get_expense(expense_id)?
            .ok_or_else(|| anyhow::anyhow!("expense {expense_id} does not exist"))?;
        if !matches!(
            expense.state,
            ExpenseState::Estimated | ExpenseState::AwaitingApproval
        ) {
            bail!("only an estimated or pending expense can be approved");
        }
        let summary = self.summary(&expense.budget_id)?;
        if otwono_types::budget::would_exceed(&summary, expense.amount_minor) {
            bail!(
                "approving this would take the budget to {} of {} {}; raise the budget first",
                (summary.committed_minor + expense.amount_minor) as f64 / 100.0,
                summary.total_minor as f64 / 100.0,
                summary.currency
            );
        }
        self.db.conn()?.execute(
            "UPDATE expenses SET state = 'approved', approved_by = ?2, approved_at = ?3 WHERE id = ?1",
            params![expense_id, approved_by, crate::now_str()],
        )?;
        self.get_expense(expense_id)?
            .ok_or_else(|| anyhow::anyhow!("expense vanished"))
    }

    pub fn set_expense_state(&self, expense_id: &str, state: ExpenseState) -> Result<Expense> {
        self.db.conn()?.execute(
            "UPDATE expenses SET state = ?2 WHERE id = ?1",
            params![expense_id, state.as_str()],
        )?;
        self.get_expense(expense_id)?
            .ok_or_else(|| anyhow::anyhow!("expense {expense_id} does not exist"))
    }

    pub fn attach_receipt(&self, expense_id: &str, receipt_path: &str) -> Result<()> {
        self.db.conn()?.execute(
            "UPDATE expenses SET receipt_path = ?2 WHERE id = ?1",
            params![expense_id, receipt_path],
        )?;
        Ok(())
    }

    pub fn summary(&self, budget_id: &str) -> Result<BudgetSummary> {
        let budget = self
            .get(budget_id)?
            .ok_or_else(|| anyhow::anyhow!("budget {budget_id} does not exist"))?;
        Ok(summarise(&budget, &self.expenses(budget_id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget(db: &Db) -> Budget {
        BudgetRepo::new(db)
            .create(None, "Project budget", "usd", 100_00, 25_00)
            .unwrap()
    }

    #[test]
    fn a_budget_is_always_simulated_and_its_currency_is_normalised() {
        let db = Db::open_in_memory().unwrap();
        let budget = budget(&db);
        assert!(budget.simulated);
        assert_eq!(budget.currency, "USD");
    }

    #[test]
    fn nonsense_budgets_are_refused() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        assert!(repo.create(None, "x", "USD", -1, 0).is_err());
        assert!(repo.create(None, "x", "DOLLARS", 100, 0).is_err());
        assert!(repo.create(None, "x", "U$D", 100, 0).is_err());
    }

    #[test]
    fn small_expenses_are_estimated_and_large_ones_await_approval() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);

        let small = repo
            .record_expense(&budget.id, None, "tools", "A licence", 10_00)
            .unwrap();
        assert_eq!(small.state, ExpenseState::Estimated);

        let large = repo
            .record_expense(&budget.id, None, "tools", "A bigger licence", 40_00)
            .unwrap();
        assert_eq!(large.state, ExpenseState::AwaitingApproval);
        assert!(large.simulated);
    }

    #[test]
    fn an_agent_cannot_pre_approve_its_own_spending() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);
        // There is no parameter that lets the caller choose the state.
        let expense = repo
            .record_expense(&budget.id, None, "tools", "x", 90_00)
            .unwrap();
        assert_eq!(expense.state, ExpenseState::AwaitingApproval);
        assert!(expense.approved_by.is_none());
    }

    #[test]
    fn only_approved_and_settled_amounts_count_against_the_budget() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);

        let a = repo
            .record_expense(&budget.id, None, "tools", "a", 10_00)
            .unwrap();
        repo.record_expense(&budget.id, None, "tools", "b", 30_00)
            .unwrap();
        repo.approve_expense(&a.id, "user").unwrap();

        let summary = repo.summary(&budget.id).unwrap();
        assert_eq!(summary.committed_minor, 10_00);
        assert_eq!(summary.pending_minor, 30_00);
        assert_eq!(summary.remaining_minor, 90_00);
        assert!(summary.simulated);
    }

    #[test]
    fn approving_beyond_the_budget_is_refused_with_a_useful_message() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);
        let first = repo
            .record_expense(&budget.id, None, "tools", "a", 95_00)
            .unwrap();
        repo.approve_expense(&first.id, "user").unwrap();

        let second = repo
            .record_expense(&budget.id, None, "tools", "b", 30_00)
            .unwrap();
        let err = repo
            .approve_expense(&second.id, "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("raise the budget first"), "{err}");
        assert_eq!(
            repo.get_expense(&second.id).unwrap().unwrap().state,
            ExpenseState::AwaitingApproval,
            "a refused approval must not change the expense"
        );
    }

    #[test]
    fn a_rejected_expense_cannot_then_be_approved() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);
        let expense = repo
            .record_expense(&budget.id, None, "tools", "a", 10_00)
            .unwrap();
        repo.set_expense_state(&expense.id, ExpenseState::Rejected)
            .unwrap();
        assert!(repo.approve_expense(&expense.id, "user").is_err());
    }

    #[test]
    fn a_receipt_can_be_attached_by_hand() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);
        let expense = repo
            .record_expense(&budget.id, None, "travel", "Train", 12_50)
            .unwrap();
        repo.attach_receipt(&expense.id, "/home/u/receipts/train.pdf")
            .unwrap();
        assert_eq!(
            repo.get_expense(&expense.id)
                .unwrap()
                .unwrap()
                .receipt_path
                .as_deref(),
            Some("/home/u/receipts/train.pdf")
        );
    }

    #[test]
    fn every_stored_expense_is_marked_simulated() {
        let db = Db::open_in_memory().unwrap();
        let repo = BudgetRepo::new(&db);
        let budget = budget(&db);
        repo.record_expense(&budget.id, None, "tools", "a", 1_00)
            .unwrap();
        let non_simulated: i64 = db
            .conn()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM expenses WHERE simulated = 0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(non_simulated, 0);
    }
}
