//! code.rs
use std::path::Path;

use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::strategy::{run_strategy_async, PhaseReq, Task, ToolScope};
use pirs_agent::Agent;
use pirs_claw::presets::{
    apply_code_defaults, build_code_agent, coding_system_prompt, coding_tools,
    resolve_code_strategy, CodeOptions,
};
use pirs_claw::registry;
use pirs_skills::{
    skills_prompt_section, Skill,
};
use pirs_claw::{
    describe_exec_backend,
    empty_assistant_diag, extract_assistant_reply, require_llm_key,
};

use super::tools::{
    chat_safe_tools, install_claw_safety, load_claw_extensions,
};

pub async fn run_code(
    cwd: &Path,
    model: &str,
    plan_model: &str,
    strategy_name: &str,
    prompt: &str,
    max_turns: Option<usize>,
    sequential: bool,
    skills: &[Skill],
    do_learn: bool,
    load_ext: bool,
) -> anyhow::Result<()> {
    let opts = apply_code_defaults(CodeOptions {
        cwd: cwd.to_path_buf(),
        model: model.into(),
        plan_model: if plan_model.is_empty() {
            None
        } else {
            Some(plan_model.into())
        },
        strategy: strategy_name.into(),
        prompt: Some(prompt.into()),
        max_turns,
        sequential,
    });

    let strategy = resolve_code_strategy(&opts)?;
    eprintln!(
        "[pirs-claw code: cwd={} model={} plan_model={:?} strategy={} phases={} exec={}]",
        opts.cwd.display(),
        opts.model,
        opts.plan_model,
        strategy.name,
        strategy.steps.len(),
        describe_exec_backend()
    );

    let retries = if sequential { 3 } else { 2 };
    let (provider, key, _) = registry::resolve_llm(&opts.model, retries)?;
    require_llm_key(key.as_deref())?;
    let host = load_claw_extensions(&opts.cwd, load_ext);
    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let skill_section = skills_prompt_section(skills);
    let project_section = pirs_tools::detect_profile(&opts.cwd).prompt_section();
    let key_for_learn = completion.api_key.clone();
    let skills_owned: Vec<Skill> = skills.to_vec();
    let host_c = host.clone();

    if strategy.name != "monolithic" && strategy.steps.len() > 1 {
        let opts_c = opts.clone();
        let provider_c = provider.clone();
        let completion_c = completion.clone();
        let skill_section_c = skill_section.clone();
        let project_section_c = project_section.clone();
        let skills_c = skills_owned.clone();
        let mut driver = AgentPhaseDriver::new(move |req: &PhaseReq| {
            let model = req.model.clone().unwrap_or_else(|| opts_c.model.clone());
            let mut tools = coding_tools(&opts_c.cwd);
            tools.extend(chat_safe_tools(&opts_c.cwd, &skills_c, false, true));
            {
                let mut seen = std::collections::HashSet::new();
                tools.retain(|t| seen.insert(t.name().to_string()));
            }
            if req.scope == ToolScope::ReadOnly {
                tools.retain(|t| {
                    matches!(
                        t.name(),
                        "read"
                            | "grep"
                            | "find"
                            | "ls"
                            | "code_map"
                            | "code_search"
                            | "recall"
                            | "skill_list"
                            | "skill_view"
                            | "web_fetch"
                            | "web_search"
                            | "project"
                            | "run_tests"
                    )
                });
            }
            let mut system = if req.system.trim().is_empty() {
                coding_system_prompt(&opts_c.cwd)
            } else {
                req.system.clone()
            };
            system.push_str(&skill_section_c);
            system.push_str(&project_section_c);
            let cwd_for_sub = opts_c.cwd.clone();
            let sub = pirs_agent::delegate::DelegateTool::new(
                provider_c.clone(),
                opts_c.model.clone(),
                completion_c.clone(),
                move || coding_tools(&cwd_for_sub),
            );
            tools.push(sub);
            let mut agent = Agent::new(provider_c.clone(), model)
                .with_system_prompt(system)
                .with_tools(tools)
                .with_completion(completion_c.clone());
            agent = install_claw_safety(agent, false, host_c.as_ref());
            if let Some(n) = opts_c.max_turns {
                agent.budgets.max_turns = Some(n);
            }
            if opts_c.sequential {
                agent = agent.with_tool_execution(pirs_agent::ExecutionMode::Sequential);
            }
            agent
        });

        let task = Task {
            issue: prompt.to_string(),
            targets: Vec::new(),
            verdict: None,
        };
        run_strategy_async(&strategy, &mut driver, &task).await?;
        let reply = extract_assistant_reply(driver.messages())
            .unwrap_or_else(|| "(strategy completed; no final assistant text)".into());
        if do_learn {
            let transcript = pirs_claw::learn::session_transcript(prompt, &reply, "code strategy run");
            let _ = pirs_claw::learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
        return Ok(());
    }

    let mut sys = coding_system_prompt(&opts.cwd);
    sys.push_str(&skill_section);
    sys.push_str(&project_section);
    let cwd_for_sub = opts.cwd.clone();
    let sub = pirs_agent::delegate::DelegateTool::new(
        provider.clone(),
        opts.model.clone(),
        completion.clone(),
        move || coding_tools(&cwd_for_sub),
    );
    let mut tools = coding_tools(&opts.cwd);
    tools.extend(chat_safe_tools(&opts.cwd, skills, false, true));
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    tools.push(sub);
    let mut agent = build_code_agent(provider.clone(), &opts)
        .with_completion(completion)
        .with_system_prompt(sys)
        .with_tools(tools);
    agent = install_claw_safety(agent, false, host.as_ref());
    let msgs = agent.prompt(prompt).await?;
    if let Some(reply) = extract_assistant_reply(&msgs) {
        if do_learn {
            let transcript = pirs_claw::learn::session_transcript(prompt, &reply, "code run");
            let _ = pirs_claw::learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
    } else {
        anyhow::bail!(
            "empty assistant reply ({})",
            empty_assistant_diag(&msgs)
        );
    }
    Ok(())
}

