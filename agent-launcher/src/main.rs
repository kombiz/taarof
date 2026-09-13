fn run() -> Result<(), String> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut session = None;
    if args.first().is_some_and(|s| s == "--session") {
        if args.len() < 2 || args[1].trim().is_empty() {
            return Err("--session requires a runtime name".into());
        }
        session = Some(args.remove(1));
        args.remove(0);
    }
    let catalog = || {
        let (catalog, warning) = agent_launcher::enriched_catalog(session.as_deref())?;
        if let Some(warning) = warning {
            eprintln!("agent: {}", agent_session_core::display_text(&warning));
        }
        Ok::<_, String>(catalog)
    };
    let picker = |mode| agent_launcher::tui::run_for_session(mode, session.as_deref());
    match args.as_slice() {
        [arg] if arg == "--build-info" => println!("{}", agent_launcher::build_info()),
        [arg] if arg == "--version" => println!("agent {}", env!("CARGO_PKG_VERSION")),
        [arg] if arg == "--help" || arg == "-h" => println!("agent — provider sessions\nUsage: agent | agent --json | --build-info | providers | doctor | --new <provider> | --resume <stable-ref> | --attach <stable-ref>\nOptional prefix: --session <Taarof runtime name>\nUse the stable_ref JSON object from --json, or an unambiguous exact session ID.\nProvider configuration controls model, approval and sandbox policy.\nAdapter authors: agent conformance <enabled-provider>."),
        [arg] if arg == "--json" => println!("{}", serde_json::to_string(&catalog()?).map_err(|e| e.to_string())?),
        [arg] if arg == "doctor" || arg == "providers" => println!("{}", agent_launcher::provider_report(arg == "doctor")?),
        [arg, target] if arg == "conformance" => println!("{}", agent_launcher::adapter_conformance(target)?),
        [arg, target] if arg == "--resume" || arg == "--attach" => {
            let kind = if arg == "--attach" { agent_session_core::ActionKind::Attach } else { agent_session_core::ActionKind::Resume };
            let plan = agent_launcher::select_action(&catalog()?, target, kind)?;
            agent_launcher::execute_plan_for_session(&plan, session.as_deref())?;
        }
        [arg, target] if arg == "--new" => {
            let plan = agent_launcher::plan_new(target, std::env::current_dir().map_err(|e|e.to_string())?)?;
            agent_launcher::execute_plan(&plan)?;
        }
        [] => picker(agent_launcher::tui::Mode::All)?,
        [arg] if arg == "--new" => picker(agent_launcher::tui::Mode::New)?,
        [arg] if arg == "--resume" => picker(agent_launcher::tui::Mode::Resume)?,
        _ => return Err("invalid arguments; see agent --help".into()),
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("agent: {}", agent_session_core::display_text(&error));
        std::process::exit(2);
    }
}
