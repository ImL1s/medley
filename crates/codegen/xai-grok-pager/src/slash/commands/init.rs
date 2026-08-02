//! `/init` -- bootstrap this repository's AGENTS.md.
//!
//! AGENTS.md is fully consumed by the harness (loaded from the git root down to
//! the working directory and prepended to every conversation) but nothing
//! generated it, so a new repository starts every session with no project
//! context at all.
//!
//! The command does no analysis of its own: it expands into an instruction the
//! model answers with its normal toolset, the same shape as `/loop`. That keeps
//! the quality of the result tied to the agent rather than to a Rust-side
//! repository scanner, and means the "an instruction file already exists" branch
//! is decided by something that can actually read the file.

use agent_client_protocol as acp;
use xai_grok_tools_api::slash_commands::{INIT_COMMAND_NAME, init_agents_md_instruction};

use crate::slash::command::{CommandExecCtx, CommandResult, SlashCommand};

/// Analyze the repository and write (or improve) its AGENTS.md.
pub struct InitCommand;

impl SlashCommand for InitCommand {
    fn name(&self) -> &str {
        INIT_COMMAND_NAME
    }

    fn description(&self) -> &str {
        "Analyze this repo and write its AGENTS.md"
    }

    /// Session-scoped: the expansion is an agent turn, so it needs a
    /// conversation to land in.
    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        "/init [focus]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    /// Args are optional -- bare `/init` is the common case.
    fn args_required(&self) -> bool {
        false
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("[focus]")
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let args = args.trim();
        let display_text = if args.is_empty() {
            "/init".to_string()
        } else {
            format!("/init {args}")
        };
        CommandResult::InjectSkill {
            display_text,
            prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                init_agents_md_instruction(args),
            ))],
            // A builtin, not a skill: no teal skill accent in scrollback.
            display_as_skill: false,
            scheduled_task_preview: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::app::bundle::BundleState;
    use crate::settings::PagerLocalSnapshot;

    static DEFAULT_BUNDLE_STATE: BundleState = BundleState {
        has_cache: false,
        version: String::new(),
        personas: Vec::new(),
        roles: Vec::new(),
        agents: Vec::new(),
        skills: Vec::new(),
        persona_details: Vec::new(),
        role_details: Vec::new(),
    };

    fn run_init(args: &str) -> CommandResult {
        let models = ModelState::default();
        let mut ctx = CommandExecCtx {
            models: &models,
            session_id: None,
            bundle_state: &DEFAULT_BUNDLE_STATE,
            screen_mode: crate::app::ScreenMode::Fullscreen,
            billing_surface_visible: true,
            usage_command_visible: true,
            pager_state: PagerLocalSnapshot::default(),
        };
        InitCommand.run(&mut ctx, args)
    }

    fn prompt_text(result: &CommandResult) -> &str {
        match result {
            CommandResult::InjectSkill { prompt_blocks, .. } => match &prompt_blocks[0] {
                acp::ContentBlock::Text(text) => &text.text,
                other => panic!("expected a text prompt block, got {other:?}"),
            },
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }

    #[test]
    fn metadata_matches_optional_arg_contract() {
        let cmd = InitCommand;
        assert_eq!(cmd.name(), "init");
        assert_eq!(cmd.usage(), "/init [focus]");
        assert!(cmd.takes_args(), "/init accepts an optional focus");
        assert!(!cmd.args_required(), "bare /init must execute");
        assert!(cmd.session_scoped(), "the expansion needs a conversation");
        assert!(cmd.aliases().is_empty());
    }

    #[test]
    fn bare_invocation_injects_the_instruction() {
        let result = run_init("");
        match &result {
            CommandResult::InjectSkill {
                display_text,
                display_as_skill,
                scheduled_task_preview,
                ..
            } => {
                assert_eq!(display_text, "/init");
                assert!(!display_as_skill, "/init is a builtin, not a skill");
                assert!(scheduled_task_preview.is_none());
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
        assert_eq!(prompt_text(&result), init_agents_md_instruction(""));
    }

    #[test]
    fn whitespace_only_args_are_treated_as_bare() {
        let result = run_init("   ");
        match &result {
            CommandResult::InjectSkill { display_text, .. } => assert_eq!(display_text, "/init"),
            other => panic!("expected InjectSkill, got {other:?}"),
        }
        assert_eq!(prompt_text(&result), init_agents_md_instruction(""));
    }

    #[test]
    fn args_reach_the_display_line_and_the_prompt() {
        let result = run_init("  focus on the test setup  ");
        match &result {
            CommandResult::InjectSkill { display_text, .. } => {
                assert_eq!(display_text, "/init focus on the test setup");
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
        let text = prompt_text(&result);
        assert!(text.contains("## Also from the user\nfocus on the test setup"));
    }

    /// Pins the full expansion. `/init` is answered by the model with no host
    /// validation of the result, so the wording *is* the feature -- a reviewer
    /// should have to look at a diff of it, not discover the change in a repo.
    #[test]
    fn instruction_text_snapshot() {
        insta::assert_snapshot!("init_instruction", init_agents_md_instruction(""));
    }
}
