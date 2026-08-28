//! Budget simulator and approval ledger.
//!
//! This module models *records of intent*, not money movement. Nothing in this
//! codebase can transfer funds; see `DECISIONS.md` D-007.

use serde::{Deserialize, Serialize};

use crate::error::{DomainError, DomainResult};
use crate::ids::Timestamp;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpenseState {
    Estimated,
    AwaitingApproval,
    Approved,
    Rejected,
    /// Recorded as spent in the simulated ledger.
    Settled,
    Cancelled,
}

impl ExpenseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Estimated => "estimated",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Settled => "settled",
            Self::Cancelled => "cancelled",
        }
    }

    /// Only approved and settled amounts consume budget.
    pub const fn consumes_budget(self) -> bool {
        matches!(self, Self::Approved | Self::Settled)
    }

    pub fn parse(value: &str) -> DomainResult<Self> {
        match value {
            "estimated" => Ok(Self::Estimated),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "settled" => Ok(Self::Settled),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(DomainError::validation(
                "expense_state",
                format!("unknown {other:?}"),
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Budget {
    pub id: String,
    pub project_id: Option<String>,
    pub name: String,
    pub currency: String,
    pub total_minor: i64,
    /// Anything at or above this needs an explicit human approval.
    pub approval_threshold_minor: i64,
    /// Always true in this build. Present so that the field is unambiguous in
    /// exports and in the API.
    pub simulated: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Expense {
    pub id: String,
    pub budget_id: String,
    pub task_id: Option<String>,
    pub category: String,
    pub description: String,
    pub amount_minor: i64,
    pub state: ExpenseState,
    /// Path to a receipt the user attached by hand.
    pub receipt_path: Option<String>,
    pub approved_by: Option<String>,
    pub approved_at: Option<Timestamp>,
    pub simulated: bool,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BudgetSummary {
    pub budget_id: String,
    pub currency: String,
    pub total_minor: i64,
    pub committed_minor: i64,
    pub pending_minor: i64,
    pub remaining_minor: i64,
    pub simulated: bool,
}

/// Decide what a newly recorded expense needs before it can proceed.
pub fn required_state_for(amount_minor: i64, budget: &Budget) -> DomainResult<ExpenseState> {
    if amount_minor < 0 {
        return Err(DomainError::validation(
            "amount_minor",
            "must not be negative",
        ));
    }
    Ok(if amount_minor >= budget.approval_threshold_minor {
        ExpenseState::AwaitingApproval
    } else {
        ExpenseState::Estimated
    })
}

/// Whether approving this expense would exceed the budget.
pub fn would_exceed(summary: &BudgetSummary, amount_minor: i64) -> bool {
    summary.committed_minor.saturating_add(amount_minor) > summary.total_minor
}

pub fn summarise(budget: &Budget, expenses: &[Expense]) -> BudgetSummary {
    let mut committed = 0i64;
    let mut pending = 0i64;
    for expense in expenses {
        if expense.state.consumes_budget() {
            committed = committed.saturating_add(expense.amount_minor);
        } else if matches!(
            expense.state,
            ExpenseState::Estimated | ExpenseState::AwaitingApproval
        ) {
            pending = pending.saturating_add(expense.amount_minor);
        }
    }
    BudgetSummary {
        budget_id: budget.id.clone(),
        currency: budget.currency.clone(),
        total_minor: budget.total_minor,
        committed_minor: committed,
        pending_minor: pending,
        remaining_minor: budget.total_minor.saturating_sub(committed),
        simulated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::now;

    fn budget(total: i64, threshold: i64) -> Budget {
        Budget {
            id: "bud_1".into(),
            project_id: None,
            name: "Test".into(),
            currency: "USD".into(),
            total_minor: total,
            approval_threshold_minor: threshold,
            simulated: true,
            created_at: now(),
        }
    }

    fn expense(amount: i64, state: ExpenseState) -> Expense {
        Expense {
            id: "exp".into(),
            budget_id: "bud_1".into(),
            task_id: None,
            category: "tools".into(),
            description: "x".into(),
            amount_minor: amount,
            state,
            receipt_path: None,
            approved_by: None,
            approved_at: None,
            simulated: true,
            created_at: now(),
        }
    }

    #[test]
    fn amounts_at_or_above_the_threshold_need_approval() {
        let b = budget(100_00, 50_00);
        assert_eq!(
            required_state_for(49_99, &b).unwrap(),
            ExpenseState::Estimated
        );
        assert_eq!(
            required_state_for(50_00, &b).unwrap(),
            ExpenseState::AwaitingApproval
        );
        assert_eq!(
            required_state_for(80_00, &b).unwrap(),
            ExpenseState::AwaitingApproval
        );
    }

    #[test]
    fn negative_amounts_are_refused() {
        assert!(required_state_for(-1, &budget(100, 10)).is_err());
    }

    #[test]
    fn only_approved_and_settled_amounts_consume_budget() {
        let b = budget(100_00, 50_00);
        let summary = summarise(
            &b,
            &[
                expense(10_00, ExpenseState::Approved),
                expense(5_00, ExpenseState::Settled),
                expense(30_00, ExpenseState::AwaitingApproval),
                expense(7_00, ExpenseState::Rejected),
                expense(9_00, ExpenseState::Cancelled),
            ],
        );
        assert_eq!(summary.committed_minor, 15_00);
        assert_eq!(summary.pending_minor, 30_00);
        assert_eq!(summary.remaining_minor, 85_00);
        assert!(summary.simulated);
    }

    #[test]
    fn overspend_is_detected_before_approval() {
        let b = budget(100_00, 10_00);
        let summary = summarise(&b, &[expense(95_00, ExpenseState::Approved)]);
        assert!(would_exceed(&summary, 6_00));
        assert!(!would_exceed(&summary, 5_00));
    }
}
