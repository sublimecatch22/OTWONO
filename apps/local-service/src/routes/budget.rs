//! Budgets and the simulated approval ledger.

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use otwono_store::repo::activity::{ActivityRepo, NewActivity};
use otwono_store::repo::budget::BudgetRepo;
use otwono_types::budget::{Budget, BudgetSummary, Expense, ExpenseState};

use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Every response carries this so no screen can render a figure without the
/// disclaimer beside it.
pub const SIMULATION_NOTICE: &str =
    "All amounts in OTWONO are simulated. No money moves, no payment is made, and nothing here \
     authorises a real purchase.";

#[derive(Debug, Serialize)]
pub struct BudgetsResponse {
    pub budgets: Vec<BudgetWithSummary>,
    pub notice: &'static str,
}

#[derive(Debug, Serialize)]
pub struct BudgetWithSummary {
    #[serde(flatten)]
    pub budget: Budget,
    pub summary: BudgetSummary,
}

pub async fn list(State(state): State<AppState>) -> ApiResult<Json<BudgetsResponse>> {
    let repo = BudgetRepo::new(&state.db);
    let mut budgets = Vec::new();
    for budget in repo.list()? {
        let summary = repo.summary(&budget.id)?;
        budgets.push(BudgetWithSummary { budget, summary });
    }
    Ok(Json(BudgetsResponse {
        budgets,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateBudget {
    pub name: String,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default = "default_currency")]
    pub currency: String,
    pub total_minor: i64,
    #[serde(default)]
    pub approval_threshold_minor: i64,
}

fn default_currency() -> String {
    "USD".into()
}

pub async fn create(
    State(state): State<AppState>,
    Json(body): Json<CreateBudget>,
) -> ApiResult<Json<BudgetWithSummary>> {
    let repo = BudgetRepo::new(&state.db);
    let budget = repo
        .create(
            body.project_id.as_deref(),
            &body.name,
            &body.currency,
            body.total_minor,
            body.approval_threshold_minor,
        )
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let summary = repo.summary(&budget.id)?;
    Ok(Json(BudgetWithSummary { budget, summary }))
}

#[derive(Debug, Serialize)]
pub struct BudgetDetail {
    #[serde(flatten)]
    pub budget: Budget,
    pub summary: BudgetSummary,
    pub expenses: Vec<Expense>,
    pub notice: &'static str,
}

pub async fn get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<BudgetDetail>> {
    let repo = BudgetRepo::new(&state.db);
    let budget = repo
        .get(&id)?
        .ok_or_else(|| ApiError::not_found("That budget"))?;
    Ok(Json(BudgetDetail {
        summary: repo.summary(&id)?,
        expenses: repo.expenses(&id)?,
        budget,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RecordExpense {
    #[serde(default = "default_category")]
    pub category: String,
    pub description: String,
    pub amount_minor: i64,
    #[serde(default)]
    pub task_id: Option<String>,
}

fn default_category() -> String {
    "general".into()
}

#[derive(Debug, Serialize)]
pub struct ExpenseResponse {
    #[serde(flatten)]
    pub expense: Expense,
    pub summary: BudgetSummary,
    pub needs_approval: bool,
    pub notice: &'static str,
}

pub async fn record_expense(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RecordExpense>,
) -> ApiResult<Json<ExpenseResponse>> {
    let repo = BudgetRepo::new(&state.db);
    let expense = repo
        .record_expense(
            &id,
            body.task_id.as_deref(),
            &body.category,
            &body.description,
            body.amount_minor,
        )
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;

    ActivityRepo::new(&state.db).record(
        NewActivity::user("budget.expense_recorded")
            .with_target("expense", &expense.id)
            .with_detail(serde_json::json!({
                "amount_minor": expense.amount_minor,
                "state": expense.state.as_str(),
                "simulated": true,
            })),
    )?;

    Ok(Json(ExpenseResponse {
        needs_approval: expense.state == ExpenseState::AwaitingApproval,
        summary: repo.summary(&id)?,
        expense,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ExpenseDecision {
    pub approve: bool,
}

pub async fn decide_expense(
    State(state): State<AppState>,
    Path((budget_id, expense_id)): Path<(String, String)>,
    Json(body): Json<ExpenseDecision>,
) -> ApiResult<Json<ExpenseResponse>> {
    let repo = BudgetRepo::new(&state.db);
    let expense = if body.approve {
        repo.approve_expense(&expense_id, "user")
            .map_err(|e| ApiError::Conflict(e.to_string()))?
    } else {
        repo.set_expense_state(&expense_id, ExpenseState::Rejected)
            .map_err(|e| ApiError::Conflict(e.to_string()))?
    };

    ActivityRepo::new(&state.db).record(
        NewActivity::user(if body.approve {
            "budget.expense_approved"
        } else {
            "budget.expense_rejected"
        })
        .with_target("expense", &expense_id),
    )?;

    Ok(Json(ExpenseResponse {
        needs_approval: false,
        summary: repo.summary(&budget_id)?,
        expense,
        notice: SIMULATION_NOTICE,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ReceiptRequest {
    pub receipt_path: String,
}

pub async fn attach_receipt(
    State(state): State<AppState>,
    Path((_budget_id, expense_id)): Path<(String, String)>,
    Json(body): Json<ReceiptRequest>,
) -> ApiResult<Json<serde_json::Value>> {
    BudgetRepo::new(&state.db).attach_receipt(&expense_id, &body.receipt_path)?;
    Ok(Json(serde_json::json!({ "attached": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn budget(state: &AppState) -> String {
        let Json(created) = create(
            State(state.clone()),
            Json(CreateBudget {
                name: "Project budget".into(),
                project_id: None,
                currency: "USD".into(),
                total_minor: 100_00,
                approval_threshold_minor: 25_00,
            }),
        )
        .await
        .unwrap();
        created.budget.id
    }

    #[tokio::test]
    async fn every_response_carries_the_simulation_notice() {
        let state = AppState::for_tests();
        let id = budget(&state).await;
        let Json(list_response) = list(State(state.clone())).await.unwrap();
        assert!(list_response.notice.contains("No money moves"));

        let Json(detail) = get(State(state), Path(id)).await.unwrap();
        assert!(detail.notice.contains("simulated"));
        assert!(detail.budget.simulated);
    }

    #[tokio::test]
    async fn a_large_expense_needs_approval_and_a_small_one_does_not() {
        let state = AppState::for_tests();
        let id = budget(&state).await;

        let Json(small) = record_expense(
            State(state.clone()),
            Path(id.clone()),
            Json(RecordExpense {
                category: "tools".into(),
                description: "A licence".into(),
                amount_minor: 10_00,
                task_id: None,
            }),
        )
        .await
        .unwrap();
        assert!(!small.needs_approval);

        let Json(large) = record_expense(
            State(state),
            Path(id),
            Json(RecordExpense {
                category: "tools".into(),
                description: "A bigger licence".into(),
                amount_minor: 40_00,
                task_id: None,
            }),
        )
        .await
        .unwrap();
        assert!(large.needs_approval);
        assert!(large.expense.simulated);
    }

    #[tokio::test]
    async fn approving_beyond_the_budget_is_a_conflict_with_an_explanation() {
        let state = AppState::for_tests();
        let id = budget(&state).await;

        let Json(first) = record_expense(
            State(state.clone()),
            Path(id.clone()),
            Json(RecordExpense {
                category: "tools".into(),
                description: "Most of it".into(),
                amount_minor: 95_00,
                task_id: None,
            }),
        )
        .await
        .unwrap();
        let _ = decide_expense(
            State(state.clone()),
            Path((id.clone(), first.expense.id)),
            Json(ExpenseDecision { approve: true }),
        )
        .await
        .unwrap();

        let Json(second) = record_expense(
            State(state.clone()),
            Path(id.clone()),
            Json(RecordExpense {
                category: "tools".into(),
                description: "Too much".into(),
                amount_minor: 30_00,
                task_id: None,
            }),
        )
        .await
        .unwrap();

        let error = decide_expense(
            State(state),
            Path((id, second.expense.id)),
            Json(ExpenseDecision { approve: true }),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ApiError::Conflict(ref m) if m.contains("raise the budget first")));
    }
}
