//! Lab experiments: run one prompt against several configurations and compare.
//!
//! A Lab never changes a production configuration. Promotion is a separate,
//! explicit act that copies a tested variant into an Office.

use anyhow::{bail, Result};

use otwono_store::repo::agents::AgentRepo;
use otwono_store::repo::workspaces::{LabResult, LabVariant, WorkspaceRepo};
use otwono_store::Db;
use otwono_types::workspace::WorkspaceKind;

use crate::executor::{AgentExecutor, AgentTurn};
use crate::prompt;

/// Highest number of variants in one experiment.
pub const MAX_VARIANTS: usize = 6;

pub struct LabRunner<'a> {
    db: &'a Db,
    executor: &'a dyn AgentExecutor,
}

impl<'a> LabRunner<'a> {
    pub fn new(db: &'a Db, executor: &'a dyn AgentExecutor) -> Self {
        Self { db, executor }
    }

    /// Run every variant against the experiment's prompt. A variant that fails
    /// records its error rather than aborting the comparison.
    pub async fn run(&self, experiment_id: &str) -> Result<Vec<LabResult>> {
        let workspaces = WorkspaceRepo::new(self.db);
        let agents = AgentRepo::new(self.db);
        let experiment = workspaces
            .get_experiment(experiment_id)?
            .ok_or_else(|| anyhow::anyhow!("experiment {experiment_id} does not exist"))?;
        if experiment.variants.is_empty() {
            bail!("this experiment has no variants to compare");
        }

        let mut results = Vec::with_capacity(experiment.variants.len());
        for variant in experiment.variants.iter().take(MAX_VARIANTS) {
            let started = std::time::Instant::now();
            let result = self.run_variant(&agents, &experiment.prompt, variant).await;
            let elapsed = started.elapsed().as_millis() as u64;

            results.push(match result {
                Ok(outcome) => LabResult {
                    variant_id: variant.id.clone(),
                    output: outcome.text,
                    error: None,
                    latency_ms: elapsed,
                    token_estimate: outcome.token_estimate,
                    ran_at: otwono_types::ids::format_ts(&otwono_types::now()),
                },
                Err(error) => LabResult {
                    variant_id: variant.id.clone(),
                    output: String::new(),
                    error: Some(error.to_string()),
                    latency_ms: elapsed,
                    token_estimate: None,
                    ran_at: otwono_types::ids::format_ts(&otwono_types::now()),
                },
            });
        }

        workspaces.save_experiment_results(experiment_id, &results, None)?;
        Ok(results)
    }

    async fn run_variant(
        &self,
        agents: &AgentRepo<'_>,
        user_prompt: &str,
        variant: &LabVariant,
    ) -> Result<crate::executor::AgentOutcome> {
        let agent = variant
            .agent_id
            .as_deref()
            .and_then(|id| agents.get(id).ok().flatten());

        let (instructions, name, role, timeout, max_tokens) = match &agent {
            Some(agent) => (
                variant
                    .system_instructions
                    .clone()
                    .unwrap_or_else(|| agent.system_instructions.clone()),
                agent.name.clone(),
                agent.role.clone(),
                agent.timeout_seconds,
                agent.parameters.max_output_tokens,
            ),
            None => (
                variant.system_instructions.clone().unwrap_or_default(),
                variant.label.clone(),
                "Lab variant".to_string(),
                120,
                None,
            ),
        };

        let model = variant
            .model
            .clone()
            .or_else(|| agent.as_ref().and_then(|a| a.model.clone()))
            .ok_or_else(|| {
                anyhow::anyhow!("variant \"{}\" has no model selected", variant.label)
            })?;

        let parts = prompt::PromptParts {
            agent_instructions: instructions,
            agent_name: Some(name.clone()),
            agent_role: Some(role),
            user_message: user_prompt.to_string(),
            ..Default::default()
        };

        self.executor
            .run(AgentTurn {
                agent_id: variant.id.clone(),
                agent_name: name,
                model,
                messages: prompt::build(&parts),
                temperature: variant
                    .temperature
                    .or_else(|| agent.as_ref().and_then(|a| a.parameters.temperature)),
                max_output_tokens: max_tokens,
                timeout_seconds: timeout,
            })
            .await
    }

    /// Copy a tested variant's settings onto an agent in an Office. This is the
    /// only route from a Lab into production, and it is explicit.
    pub fn promote(
        &self,
        experiment_id: &str,
        variant_id: &str,
        target_agent_id: &str,
    ) -> Result<otwono_types::agent::Agent> {
        let workspaces = WorkspaceRepo::new(self.db);
        let agents = AgentRepo::new(self.db);
        let experiment = workspaces
            .get_experiment(experiment_id)?
            .ok_or_else(|| anyhow::anyhow!("experiment {experiment_id} does not exist"))?;
        let variant = experiment
            .variants
            .iter()
            .find(|variant| variant.id == variant_id)
            .ok_or_else(|| {
                anyhow::anyhow!("variant {variant_id} is not part of this experiment")
            })?;

        let has_result = experiment
            .results
            .iter()
            .any(|result| result.variant_id == variant_id && result.error.is_none());
        if !has_result {
            bail!(
                "that variant has not produced a successful result yet, so it cannot be promoted"
            );
        }

        let mut target = agents
            .get(target_agent_id)?
            .ok_or_else(|| anyhow::anyhow!("agent {target_agent_id} does not exist"))?;
        if let Some(workspace_id) = &target.workspace_id {
            if let Some(workspace) = workspaces.get(workspace_id)? {
                if workspace.kind == WorkspaceKind::Lab {
                    bail!("promote into an Office, not back into a Lab");
                }
            }
        }

        if let Some(instructions) = &variant.system_instructions {
            target.system_instructions = instructions.clone();
        }
        if let Some(model) = &variant.model {
            target.model = Some(model.clone());
        }
        if let Some(connection) = &variant.provider_connection_id {
            target.provider_connection_id = Some(connection.clone());
        }
        if let Some(temperature) = variant.temperature {
            target.parameters.temperature = Some(temperature);
        }

        let updated = agents.update(
            &target,
            Some(&format!("promoted from lab variant \"{}\"", variant.label)),
        )?;
        workspaces.save_experiment_results(experiment_id, &experiment.results, Some(variant_id))?;
        Ok(updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::scripted::ScriptedExecutor;
    use otwono_store::repo::agents::NewAgent;
    use otwono_store::repo::workspaces::NewWorkspace;

    fn variant(id: &str, label: &str, instructions: &str) -> LabVariant {
        LabVariant {
            id: id.into(),
            label: label.into(),
            agent_id: None,
            provider_connection_id: None,
            model: Some("test-model".into()),
            system_instructions: Some(instructions.into()),
            temperature: Some(0.2),
        }
    }

    #[tokio::test]
    async fn every_variant_runs_and_results_are_recorded_together() {
        let db = Db::open_in_memory().unwrap();
        let workspaces = WorkspaceRepo::new(&db);
        let lab = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Prompt lab"))
            .unwrap();
        let experiment = workspaces
            .create_experiment(
                &lab.id,
                "Tone",
                "Summarise the policy.",
                &[
                    variant("v1", "Terse", "Be terse."),
                    variant("v2", "Full", "Explain fully."),
                ],
            )
            .unwrap();

        let executor = ScriptedExecutor::with_replies(vec!["Short answer.", "A longer answer."]);
        let results = LabRunner::new(&db, &executor)
            .run(&experiment.id)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].output, "Short answer.");
        assert!(results.iter().all(|r| r.error.is_none()));
        assert_eq!(
            workspaces
                .get_experiment(&experiment.id)
                .unwrap()
                .unwrap()
                .results
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn one_failing_variant_does_not_abandon_the_comparison() {
        let db = Db::open_in_memory().unwrap();
        let workspaces = WorkspaceRepo::new(&db);
        let lab = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Prompt lab"))
            .unwrap();
        let experiment = workspaces
            .create_experiment(
                &lab.id,
                "Tone",
                "Summarise.",
                &[variant("v1", "A", "a"), variant("v2", "B", "b")],
            )
            .unwrap();

        let executor = ScriptedExecutor::with_results(vec![
            Err("the model was unreachable".into()),
            Ok(crate::executor::AgentOutcome::text("It worked")),
        ]);
        let results = LabRunner::new(&db, &executor)
            .run(&experiment.id)
            .await
            .unwrap();

        assert_eq!(results.len(), 2);
        assert!(results[0].error.as_deref().unwrap().contains("unreachable"));
        assert_eq!(results[1].output, "It worked");
    }

    #[tokio::test]
    async fn an_untested_variant_cannot_be_promoted() {
        let db = Db::open_in_memory().unwrap();
        let workspaces = WorkspaceRepo::new(&db);
        let agents = AgentRepo::new(&db);
        let lab = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Lab"))
            .unwrap();
        let experiment = workspaces
            .create_experiment(
                &lab.id,
                "Tone",
                "Summarise.",
                &[variant("v1", "A", "Be terse.")],
            )
            .unwrap();
        let target = agents
            .create(NewAgent {
                name: "Writer".into(),
                ..Default::default()
            })
            .unwrap();

        let executor = ScriptedExecutor::with_replies(vec![]);
        let runner = LabRunner::new(&db, &executor);
        let error = runner
            .promote(&experiment.id, "v1", &target.id)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("has not produced a successful result"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn promotion_copies_the_tested_settings_and_records_why() {
        let db = Db::open_in_memory().unwrap();
        let workspaces = WorkspaceRepo::new(&db);
        let agents = AgentRepo::new(&db);
        let lab = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Lab"))
            .unwrap();
        let office = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Office, "Ops"))
            .unwrap();
        let experiment = workspaces
            .create_experiment(
                &lab.id,
                "Tone",
                "Summarise.",
                &[variant("v1", "Terse", "Be terse.")],
            )
            .unwrap();

        let executor = ScriptedExecutor::with_replies(vec!["Short."]);
        let runner = LabRunner::new(&db, &executor);
        runner.run(&experiment.id).await.unwrap();

        let target = agents
            .create(NewAgent {
                name: "Writer".into(),
                system_instructions: "Old instructions.".into(),
                workspace_id: Some(office.id.clone()),
                ..Default::default()
            })
            .unwrap();

        let promoted = runner.promote(&experiment.id, "v1", &target.id).unwrap();
        assert_eq!(promoted.system_instructions, "Be terse.");
        assert_eq!(promoted.model.as_deref(), Some("test-model"));
        assert_eq!(promoted.version, 2);

        let versions = agents.versions(&target.id).unwrap();
        assert!(versions[0]
            .note
            .as_deref()
            .unwrap()
            .contains("promoted from lab variant"));
        assert_eq!(
            workspaces
                .get_experiment(&experiment.id)
                .unwrap()
                .unwrap()
                .promoted_variant
                .as_deref(),
            Some("v1")
        );
    }

    #[tokio::test]
    async fn promoting_back_into_a_lab_is_refused() {
        let db = Db::open_in_memory().unwrap();
        let workspaces = WorkspaceRepo::new(&db);
        let agents = AgentRepo::new(&db);
        let lab = workspaces
            .create(NewWorkspace::named(WorkspaceKind::Lab, "Lab"))
            .unwrap();
        let experiment = workspaces
            .create_experiment(
                &lab.id,
                "Tone",
                "Summarise.",
                &[variant("v1", "Terse", "Be terse.")],
            )
            .unwrap();
        let executor = ScriptedExecutor::with_replies(vec!["Short."]);
        let runner = LabRunner::new(&db, &executor);
        runner.run(&experiment.id).await.unwrap();

        let in_lab = agents
            .create(NewAgent {
                name: "Draft".into(),
                workspace_id: Some(lab.id.clone()),
                ..Default::default()
            })
            .unwrap();
        let error = runner
            .promote(&experiment.id, "v1", &in_lab.id)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not back into a Lab"), "{error}");
    }
}
