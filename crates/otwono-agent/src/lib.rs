//! The OTWONO assistant, in the shape this machine can afford.
//!
//! `AI-RUNTIME.md` §6 gives every tier an assistant shape. This crate implements the first
//! one — [`AssistantShape::CommandGrammar`], what a T0 node gets — and the refusal that
//! every shape owes the user when it is asked for something it cannot do.
//!
//! # Why a T0 assistant exists at all
//!
//! A Pi Zero cannot run a language model. The tempting conclusion is that it has no
//! assistant, and that conclusion is what this crate exists to refuse. Most of what people
//! ask an OS assistant to do is not open-ended reasoning; it is *naming an operation and
//! its arguments*. "Put this file in my store." "What tier is this machine?" "Show me what
//! my neighbours are offering." Those are verbs, and a verb table answers them exactly as
//! well on a Pi Zero as on a workstation — faster, in fact, and with no chance of a
//! confident wrong answer.
//!
//! What a T0 node cannot do is understand a sentence it has no verb for. So the whole
//! design question is what happens *then*, and the answer is [`Refusal`]: say plainly that
//! this machine cannot, say what would be needed, and say whether somewhere else could.
//!
//! # What this crate is not
//!
//! Not a planner, not a tool registry, and not a daemon. `otwono-agentd` and its typed tool
//! registry are Phase 7 (`OTWONO-ARCHITECTURE.md` §6.3); this is the grammar those will
//! reuse, kept as a library so it is testable without booting anything.
//!
//! It also **executes nothing**. Parsing produces an [`Intent`] — a named operation with
//! typed arguments — and the caller decides what to do with it. That separation is not
//! ceremony: it means the grammar can be exercised exhaustively in unit tests with no
//! sockets, no capabilities and no daemons, and it means an intent must still pass through
//! `otwono-permd` like every other privileged action. An assistant that could act because
//! it parsed something would be an assistant with ambient privilege, which CLAUDE.md §2.5
//! forbids.

#![forbid(unsafe_code)]

pub mod grammar;
pub mod intent;
pub mod refusal;

pub use grammar::{parse, Grammar, Verb};
pub use intent::{Argument, Intent};
pub use refusal::{Elsewhere, Refusal};

pub use otwono_capability::AssistantShape;
