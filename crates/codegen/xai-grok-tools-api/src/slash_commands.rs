//! Canonical slash-command wording (`/loop`, `/imagine`, `/imagine-video`,
//! `/goal`, `/init`), shared by every front-end (Grok Build shell/pager and
//! other hosts) so expansions cannot drift.

/// Canonical tool name advertised by the scheduler create tool. Gating code
/// (shell `CommandAvailability`, pager `required_tools`, host command lists)
/// keys `/loop` availability on this name.
pub const SCHEDULER_CREATE_TOOL_NAME: &str = "scheduler_create";

/// Usage hint shown when `/loop` is invoked with no arguments.
pub fn loop_usage_message() -> &'static str {
    "Usage: /loop [interval] <prompt>\n\
     Example: /loop 30m check deploy status\n\
     Example: /loop check deploy status every hour\n\n\
     Tell me how often it should run (e.g. 30m, 1 hour, every 2 days)."
}

/// Where a scheduled fire runs, which decides what the stored prompt can rely on.
///
/// Resolved from `[scheduler] background_loops` (env, config, managed policy and
/// remote settings all feed it), so `/loop` describes the runtime the user
/// actually has rather than hedging across both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopFireMode {
    /// Each fire runs in a detached background subagent that cannot see this
    /// conversation. The default.
    Detached,
    /// Each fire runs as a turn in this conversation, where earlier results from
    /// the same task may still be visible.
    InSession,
}

/// Build the model instruction that `/loop` expands into for `args`.
///
/// The model, not brittle host parsing, turns the request into the
/// `scheduler_create` interval, accepting every natural phrasing and erroring
/// on bad input rather than silently defaulting. See [`loop_usage_message`].
///
/// Only the framing differs by `mode`; the stop condition and length guidance
/// are identical, because both hold wherever the fire runs.
pub fn loop_schedule_instruction(args: &str, mode: LoopFireMode) -> String {
    let fire_context = match mode {
        LoopFireMode::Detached => {
            "Each fire runs in a detached background subagent, not in this conversation,\n\
             so the prompt you store must stand on its own.\n\n\
             ## Writing a prompt that survives a fresh fire\n\
             - Inline the state a fire needs: paths, job/PR/branch ids, the command that checks\n\
               status, and what \"healthy\" looks like. A fire cannot see this conversation, and\n\
               a long-running task restarts from a short summary every few iterations.\n\
             - Only a short status comes back here, so say what that status must contain."
        }
        LoopFireMode::InSession => {
            "Each fire arrives as a new turn in this conversation, and earlier results from\n\
             the same task may still be above it. The stored prompt is re-sent verbatim every\n\
             time, so write a standing order rather than a one-off request.\n\n\
             ## Writing a prompt that reads well on every fire\n\
             - Name the state that must not be guessed: paths, job/PR/branch ids, the command\n\
               that checks status, and what \"healthy\" looks like. This conversation is\n\
               compacted as it grows, so do not rely on details staying visible.\n\
             - Earlier fires may be above you: continue from them instead of restarting."
        }
    };
    format!(
        "# /loop -- schedule a recurring prompt\n\n\
         Turn the input below into a scheduler_create call. {fire_context}\n\
         - Say what one fire does and when it bails: \"if still pending, report one line and\n\
           stop.\" A fire must not poll inline.\n\
         - Give it a stop condition and an exit: \"when <condition> holds, report it and call\n\
           scheduler_delete <task_id>.\" Without that the loop runs until it expires.\n\
         - Keep it short and concrete -- the stored prompt is re-sent on every fire.\n\n\
         ## Deriving the interval\n\
         Convert the user's cadence -- however phrased, at either end of the request -- into a\n\
         compact `<number><unit>` string (`s`/`m`/`h`/`d`); the remaining text is the prompt.\n\
         The minimum is 60 seconds and shorter values are raised, so say so when it applies.\n\
         If no cadence is given, ask the user how often it should run -- never invent one.\n\n\
         ## Action\n\
         Schedule from what the user already gave you \u{2014} do not explore the workspace or run\n\
         checks before scheduling; the first fire does that.\n\
         1. Call scheduler_create with the interval, the prompt, and fire_immediately: true.\n\
            If the interval is rejected, fix the string rather than guessing.\n\
         2. Confirm what's scheduled, the cadence, its stop condition, that it auto-expires\n\
            after 7 days, and the task_id to cancel with scheduler_delete.\n\
         3. Do NOT execute the prompt inline. The scheduler fires it immediately.\n\n\
         ## Wrong tool for the job\n\
         - \"Tell me when X finishes\" -> a background command or watch tool that wakes you on\n\
           the event, not a recurring loop that re-checks on a timer.\n\
         - \"Do X once in N minutes\" -> background `sleep <secs> && <command>`; scheduling is\n\
           recurring-only.\n\n\
         ## Changing an existing loop\n\
         Call scheduler_create with its task_id and only the changed fields; do not\n\
         delete and recreate. If later work changes what a loop should do, update its\n\
         prompt the same way.\n\n\
         ## Input\n\
         {args}"
    )
}

/// Canonical name of the image generation tool; gates `/imagine`.
pub const IMAGE_GEN_TOOL_NAME: &str = "image_gen";

/// Advertised name of the /imagine command.
pub const IMAGINE_COMMAND_NAME: &str = "imagine";

/// Canonical name of the image-to-video tool; gates `/imagine-video`.
pub const IMAGE_TO_VIDEO_TOOL_NAME: &str = "image_to_video";

/// Advertised name of the /imagine-video command.
pub const IMAGINE_VIDEO_COMMAND_NAME: &str = "imagine-video";

/// Usage hint shown when `/imagine` is invoked with no arguments.
pub fn imagine_usage_message() -> &'static str {
    "Usage: /imagine <description>\n\
     Provide a text description to generate an image."
}

/// Build the model instruction that `/imagine` expands into for `prompt`.
pub fn imagine_instruction(prompt: &str) -> String {
    format!(
        "Call the image_gen tool immediately, passing the user's prompt below \
         verbatim — do not rewrite, embellish, or expand it. \
         After the tool completes, briefly acknowledge and mention \
         where the image was saved.\n\n\
         Prompt: {prompt}"
    )
}

/// Usage hint shown when `/imagine-video` is invoked with no arguments.
pub fn imagine_video_usage_message() -> &'static str {
    "Usage: /imagine-video <description>\n\
     Provide a text description to generate a video."
}

/// Build the model instruction that `/imagine-video` expands into for `prompt`.
pub fn imagine_video_instruction(prompt: &str) -> String {
    format!(
        "{IMAGINE_VIDEO_SKILL}\n\n\
         User prompt: {prompt}"
    )
}

/// Video workflow guidance injected by `/imagine-video`.
const IMAGINE_VIDEO_SKILL: &str = "\
# Imagine Video

Video starts from an image — there is no text-to-video tool. \
Default to `image_to_video`; use `reference_to_video` only when the user \
explicitly asks for it or a shot genuinely needs multiple reference images.

## Default: single clip

Unless the user asks for a long video, multiple scenes, or a multi-shot sequence, \
generate **one** video:

1. Create a source image with `image_gen` that stages the first frame \
(composition, subject, lighting).
2. Call `image_to_video` with that image and a short prompt describing the motion \
or camera move (1–2 sentences, present tense).
3. After the tool completes, mention the saved file path so the user can find it.

## Longer / multi-shot videos

When the user requests a longer video, multiple scenes, or a narrative sequence:

1. **Plan the story as shots** — break the idea into distinct shots, one beat each.
2. **Favor frequent, short shots** — prefer more 6s clips over fewer long ones; more cuts keep it dynamic.
3. **Create each shot's source image** with `image_gen` (or `image_edit` to combine references), keeping characters and settings consistent across shots.
4. **Animate each shot with `image_to_video`** — the source image becomes frame 1.
5. **Assemble with FFmpeg** using stream copy (`ffmpeg -f concat ... -c copy` — never re-encode). \
Keep every shot at the same resolution and frame rate so the concat works. \
After assembly, mention the final output path.

## Shot guidance

- **Prompt-craft:** one short, vivid moment in present tense with a clear camera movement, in 1–2 sentences.
- **Minimal but interesting:** one clear subject, one simple motion or camera move per shot. Avoid complex multi-action animation; make the shot compelling through composition, lighting, and a strong moment.
- **Complex source image?** Intricate frames (busy geometry, fine detail, heavy reflections) warp when animated. Keep the subject fixed and move only the camera (slow push-in, orbit, or parallax), or break into simpler shots. For new shots, generate a simpler, animation-friendly base image rather than animating a busy one.
- **`image_to_video` animates from frame 1** — stage the first frame with `image_gen`/`image_edit` before animating.
- **Aspect ratio:** set it on the source image (`image_gen` `aspect_ratio`); don't re-crop an existing video.
- **Duration:** 6s or 10s only (prefer 6s); round to the nearest.
- **Real people:** reference-first — drive the video from a verified reference image; never animate a named person without one.
- Don't loop the same clip unless asked.";

pub const UPDATE_GOAL_TOOL_NAME: &str = "update_goal";

pub const WORKFLOW_TOOL_NAME: &str = "workflow";

pub const GOAL_COMMAND_NAME: &str = "goal";

/// Bare subcommand tokens reserved for goal lifecycle control rather than
/// being treated as an objective, matching the shell's /goal grammar.
pub const GOAL_RESERVED_SUBCOMMANDS: &[&str] = &["status", "pause", "resume", "clear", "edit"];

pub fn goal_usage_message() -> &'static str {
    "Usage: /goal <objective>\n\
     Set an objective to work toward until it is complete."
}

pub fn goal_instruction(objective: &str) -> String {
    format!(
        "# /goal -- pursue an objective\n\n\
         A goal has been set: {objective}\n\n\
         Work directly on this goal and carry it as far as you can. Deliver \
         everything the user asked for yourself: no follow-up questions, no \
         manual steps left for the user. If the conversation continues, keep \
         pursuing the goal until it is complete.\n\n\
         TRACKING: break the objective into concrete steps and track them \
         (use your todo tool if one is available), marking each done as you \
         finish it.\n\n\
         VERIFY AS YOU GO: test each change on the real path before moving on. \
         A completion claim must be backed by evidence produced in this \
         session, not assumptions.\n\n\
         Call update_goal(completed: true, message: \"summary\") ONLY when the \
         goal is fully achieved. Call update_goal(blocked_reason: \"reason\") \
         only when truly stuck after 3+ consecutive failed attempts at the \
         same problem. Call update_goal(message: \"status note\") to log \
         progress along the way. If update_goal returns an error, continue \
         working the goal and report status in your reply instead.\n\n\
         Start now."
    )
}

/// Advertised name of the /init command.
pub const INIT_COMMAND_NAME: &str = "init";

/// Build the model instruction that `/init` expands into.
///
/// `/init` writes no file itself: it hands the model an assignment and lets the
/// normal toolset do the reading and writing, so the analysis is as good as the
/// session's tools rather than as good as a hard-coded Rust scanner.
///
/// The "already exists" branch is delegated to the model on purpose. A host-side
/// `AGENTS.md.exists()` check would miss every other name the loader accepts
/// (`AGENT.md`, `CLAUDE.md`, `.claude/CLAUDE.md`) and every copy above the
/// working directory, and it would be checking a different directory than the
/// one the model ends up working in.
///
/// `args` is optional extra direction from the user ("focus on the test setup");
/// empty args produce no trailing section.
pub fn init_agents_md_instruction(args: &str) -> String {
    let extra = args.trim();
    let extra_section = if extra.is_empty() {
        String::new()
    } else {
        format!("\n\n## Also from the user\n{extra}")
    };
    format!(
        "# /init -- write this repository's AGENTS.md\n\n\
         Produce the AGENTS.md that every future session in this repository starts with.\n\
         It is loaded automatically from the git root down to the working directory and\n\
         prepended to every conversation, so its length is a permanent cost: make it earn it.\n\n\
         ## 1. Find what already exists -- never overwrite blindly\n\
         Check the repository root and the working directory for an instruction file the\n\
         loader already reads: AGENTS.md, AGENT.md, CLAUDE.md, or .claude/CLAUDE.md. Also\n\
         look for rules other tools left behind: .grok/rules/, .claude/rules/, .cursor/rules/,\n\
         .github/copilot-instructions.md, CONTRIBUTING.md.\n\n\
         If one of those instruction files exists, READ IT FIRST and treat it as the source of\n\
         truth. Improve it in place: correct what is wrong, add what is missing, delete what\n\
         the code no longer does. Do not regenerate it from scratch, and do not drop a rule you\n\
         cannot show is obsolete -- someone put it there for a reason you may not have hit yet.\n\
         If no instruction file exists, create AGENTS.md at the repository root. Fold anything\n\
         useful from the other tools' rule files into it rather than leaving it stranded.\n\n\
         ## 2. Investigate before you write\n\
         Every line you write must come from a file you actually read, not from how projects\n\
         like this usually work. At minimum read the README and CONTRIBUTING, the build and\n\
         package manifests, the CI workflow (usually the most reliable source for the real\n\
         build/test/lint commands), the linter and formatter configs, and enough of the source\n\
         tree to describe the architecture honestly. Prefer confirming a command exists over\n\
         asserting it does.\n\n\
         ## 3. What the file must cover\n\
         In this order, skipping any heading that genuinely does not apply:\n\n\
         - Overview -- one or two sentences: what this project is and what it produces.\n\
         - Commands -- the exact build, test, lint, format, and run invocations,\n  \
           copy-pasteable. Include how to run a SINGLE test or one package's tests; that is\n  \
           usually the most valuable line in the file. Note any environment variable, toolchain\n  \
           version manager, or setup step that is easy to miss and expensive to get wrong.\n\
         - Architecture -- the things that take an hour to work out from the source: what the\n  \
           top-level modules/crates/packages own, where the entry points are, how a request or\n  \
           build flows through them, and which directories are generated or vendored and must\n  \
           never be hand-edited.\n\
         - Conventions -- this project's own style where it departs from the language default:\n  \
           naming, error handling, logging, test layout, import ordering. Only what a reviewer\n  \
           would actually reject a patch over.\n\
         - Gotchas -- the rules that cause real breakage: pre-commit hooks, generated files,\n  \
           migration ordering, platform-specific steps, tests that need a flag or a long\n  \
           timeout. State the commit-message or PR convention if the repo has one.\n\n\
         ## 4. Keep it earned\n\
         - Aim for roughly 30-60 lines. Density beats coverage.\n\
         - Write instructions, not description: \"run `<test command> <one test>`\", not \"this\n  \
           project has tests\".\n\
         - Cut anything an agent finds faster by looking: file listings, dependency dumps,\n  \
           paraphrases of the README.\n\
         - Cut transient state: open bugs, in-flight work, TODOs. It goes stale silently.\n\
         - Do not invent a convention the code does not follow. An unverified rule is worse\n  \
           than a missing one.\n\
         - In a monorepo keep the root file general and put package-specific rules in that\n  \
           package's own AGENTS.md -- deeper files are loaded too and win on conflict.\n\n\
         ## 5. Finish\n\
         Write the file, then report its path and a short summary of what went in it. If you\n\
         improved an existing file, say exactly what you changed and why.{extra_section}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imagine_instruction_carries_prompt_verbatim() {
        let text = imagine_instruction("a golden sunset");
        assert!(text.contains("a golden sunset"));
        assert!(text.contains("image_gen"));
        assert!(text.contains("verbatim"));
    }

    #[test]
    fn imagine_video_instruction_carries_prompt_and_workflow() {
        let text = imagine_video_instruction("a cat playing piano");
        assert!(text.contains("a cat playing piano"));
        assert!(text.contains("image_to_video"));
        assert!(text.contains("FFmpeg"));
    }

    #[test]
    fn instruction_carries_args_and_contract_tokens() {
        for mode in [LoopFireMode::Detached, LoopFireMode::InSession] {
            let text = loop_schedule_instruction("every 30 minutes do x", mode);
            assert!(text.contains("every 30 minutes do x"), "{mode:?}");
            assert!(text.contains("<number><unit>"), "{mode:?}");
            assert!(text.contains("ask the user how often"), "{mode:?}");
            assert!(
                !text.contains("10m"),
                "no host-side default interval: {mode:?}"
            );
            assert!(
                !text.contains("recurring:"),
                "the retired one-shot flag must not be referenced: {mode:?}"
            );
            assert!(
                text.contains("task_id"),
                "must teach in-place updates via task_id: {mode:?}"
            );
            assert!(
                text.contains("delete and recreate"),
                "must steer away from delete+recreate: {mode:?}"
            );
            assert!(
                text.contains("scheduler_delete <task_id>"),
                "every mode must authorize the fire to end the task: {mode:?}"
            );
        }
    }

    #[test]
    fn each_fire_mode_describes_its_own_runtime() {
        let detached = loop_schedule_instruction("5m check ci", LoopFireMode::Detached);
        let in_session = loop_schedule_instruction("5m check ci", LoopFireMode::InSession);

        assert!(detached.contains("cannot see this conversation"));
        assert!(!detached.contains("arrives as a new turn in this conversation"));

        assert!(in_session.contains("arrives as a new turn in this conversation"));
        assert!(!in_session.contains("cannot see this conversation"));

        // The two levers the A/B showed carry the behavior are mode-independent.
        for text in [&detached, &in_session] {
            assert!(text.contains("report it and call"));
            assert!(text.contains("Keep it short and concrete"));
        }
    }

    #[test]
    fn goal_instruction_carries_objective_and_contract_tokens() {
        let text = goal_instruction("ship the widget");
        assert!(text.contains("ship the widget"));
        assert!(text.contains("update_goal(completed: true"));
        assert!(text.contains("blocked_reason"));
        assert!(text.contains("If update_goal returns an error"));
        assert!(
            !text.contains("system-reminder"),
            "expansions ride as user messages and must not claim reminder authority"
        );
        assert!(goal_usage_message().contains("Usage: /goal"));
    }

    #[test]
    fn usage_message_has_no_default_claim() {
        assert!(loop_usage_message().contains("Usage: /loop"));
        assert!(!loop_usage_message().contains("10m"));
    }

    #[test]
    fn init_instruction_carries_contract_tokens() {
        let text = init_agents_md_instruction("");

        // The deliverable and where it goes.
        assert!(text.contains("AGENTS.md"));
        assert!(text.contains("repository root"));

        // Every name the loader accepts must be searched, or the "already
        // exists" branch silently misses repos that use a different one.
        for name in ["AGENTS.md", "AGENT.md", "CLAUDE.md", ".claude/CLAUDE.md"] {
            assert!(text.contains(name), "missing existing-file name {name}");
        }

        // The non-destructive contract for an existing file.
        assert!(text.contains("never overwrite blindly"));
        assert!(text.contains("READ IT FIRST"));
        assert!(text.contains("Do not regenerate it from scratch"));

        // The sections that make the file worth its permanent context cost.
        assert!(text.contains("SINGLE test"));
        assert!(text.contains("generated or vendored"));

        // Expansions ride as user messages and must not claim reminder authority.
        assert!(!text.contains("system-reminder"));
    }

    /// Rust's `\`-continuation eats the leading whitespace of the next source
    /// line, so a wrapped bullet silently loses its markdown indent. Structural
    /// guard: inside a bullet, a continuation line must be indented.
    #[test]
    fn init_instruction_indents_wrapped_bullets() {
        let text = init_agents_md_instruction("");
        let mut in_bullet = false;
        for line in text.lines() {
            if line.starts_with("- ") {
                in_bullet = true;
            } else if line.is_empty() || line.starts_with('#') {
                in_bullet = false;
            } else if in_bullet {
                assert!(
                    line.starts_with("  "),
                    "unindented bullet continuation: {line:?}"
                );
            }
        }
    }

    #[test]
    fn init_instruction_appends_user_direction_only_when_present() {
        let bare = init_agents_md_instruction("");
        assert!(!bare.contains("Also from the user"));
        assert_eq!(bare, init_agents_md_instruction("   "));

        let focused = init_agents_md_instruction("focus on the test setup");
        assert!(focused.contains("## Also from the user\nfocus on the test setup"));
        assert!(focused.starts_with(&bare));
    }
}
