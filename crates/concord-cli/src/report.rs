//! Human-readable rendering of analyses.

use crate::{Analysis, Simulation};
use concord_core::{BranchFootprint, ChangeKind, Finding, Symbol};
use std::fmt::Write;

pub fn symbols(path: &str, symbols: &[Symbol]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{path}");
    for s in symbols {
        let _ = writeln!(
            out,
            "  {:<8} {:<40} L{:<4} contract={} body={}\n           {}",
            format!("{:?}", s.kind).to_lowercase(),
            s.id.as_str(),
            s.span.start_line,
            s.contract.digest.short(),
            s.body.short(),
            s.contract.display
        );
    }
    out
}

fn change_line(out: &mut String, c: &concord_core::SymbolChange) {
    let (tag, extra) = match &c.kind {
        ChangeKind::Added => ("+ added   ", String::new()),
        ChangeKind::Removed => ("- removed ", String::new()),
        ChangeKind::Renamed { to } => ("~ renamed ", format!(" -> {to}")),
        ChangeKind::ContractChanged => ("! contract", String::new()),
        ChangeKind::BodyChanged => ("  body    ", String::new()),
    };
    let _ = writeln!(out, "    {tag} {}{extra}", c.symbol);
    if c.is_contract_change() {
        if let Some(b) = &c.before {
            let _ = writeln!(out, "                 before: {}", b.display);
        }
        match &c.after {
            Some(a) => {
                let _ = writeln!(out, "                 after : {}", a.display);
            }
            None => {
                let _ = writeln!(out, "                 after : (removed)");
            }
        }
    }
}

fn branch(out: &mut String, b: &BranchFootprint) {
    let contract = b.changes.iter().filter(|c| c.is_contract_change()).count();
    let body = b.changes.iter().filter(|c| c.is_body_change()).count();
    let added = b.changes.iter().filter(|c| matches!(c.kind, ChangeKind::Added)).count();
    let _ = writeln!(
        out,
        "  {}: {} file(s), {} contract / {} body / {} added symbol(s), depends on {} symbol(s)",
        b.name,
        b.changed_files.len(),
        contract,
        body,
        added,
        b.uses.len()
    );
    for c in &b.changes {
        change_line(out, c);
    }
}

pub fn analysis(a: &Analysis, sim: Option<&Simulation>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Concord analysis   base: {} ({})   branches: {}\n",
        a.base,
        &a.base_commit[..a.base_commit.len().min(10)],
        a.branches
            .iter()
            .map(|b| b.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let _ = writeln!(out, "Branches");
    for b in &a.branches {
        branch(&mut out, b);
    }

    let _ = writeln!(out, "\nFindings");
    if a.findings.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for f in &a.findings {
        match f {
            Finding::ContractDependency {
                producer,
                consumer,
                symbol,
                after,
                sites,
                file_disjoint,
                ..
            } => {
                let _ = writeln!(
                    out,
                    "  CONTRACT   {producer} -> {consumer}: {symbol}{}",
                    if *file_disjoint {
                        "   (no shared files: invisible to textual merge and file locks)"
                    } else {
                        ""
                    }
                );
                let _ = writeln!(
                    out,
                    "             now: {}",
                    after.as_ref().map(|c| c.display.as_str()).unwrap_or("(removed)")
                );
                for s in sites {
                    let _ = writeln!(out, "             {}:{}  {}: {}", s.path, s.line, s.from.name(), s.text);
                }
            }
            Finding::FileOverlap {
                a: x,
                b: y,
                path,
                shared_symbols,
            } => {
                if shared_symbols.is_empty() {
                    let _ = writeln!(
                        out,
                        "  FILE       {x} & {y}: {path}   disjoint symbols (a file lock would serialise this needlessly)"
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "  FILE       {x} & {y}: {path}   shared symbols: {}",
                        shared_symbols.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                    );
                }
            }
            Finding::SameSymbol {
                a: x,
                b: y,
                symbol,
                a_change,
                b_change,
            } => {
                let _ = writeln!(out, "  SYMBOL     {x} & {y}: {symbol}   ({a_change:?} / {b_change:?})");
            }
            Finding::DuplicateDefinition { a: x, b: y, symbol } => {
                let _ = writeln!(out, "  DUPLICATE  {x} & {y}: both add {symbol}");
            }
        }
    }

    if !a.textual.is_empty() {
        let _ = writeln!(out, "\nTextual merge (git merge-tree)");
        for t in &a.textual {
            if t.clean {
                let _ = writeln!(out, "  {} & {}: clean", t.a, t.b);
            } else {
                let _ = writeln!(
                    out,
                    "  {} & {}: CONFLICT in {}",
                    t.a,
                    t.b,
                    t.conflicted_files.join(", ")
                );
            }
        }
    }

    let s = &a.summary;
    let _ = writeln!(
        out,
        "\nSummary: {} contract dependencies ({} across disjoint files), {} file overlaps ({} symbol-disjoint), \
         {} same-symbol edits ({} body-only), {} duplicate definitions",
        s.contract_dependencies,
        s.contract_dependencies_file_disjoint,
        s.file_overlaps,
        s.file_overlaps_symbol_disjoint,
        s.same_symbol,
        s.same_symbol_both_body_only,
        s.duplicate_definitions
    );

    if let Some(sim) = sim {
        let _ = writeln!(out, "\nProtocol simulation (notices each agent would receive)");
        if sim.notices.is_empty() {
            let _ = writeln!(out, "  (no notices)");
        }
        for n in &sim.notices {
            let _ = writeln!(out, "  -> to {}", n.to);
            for line in n.render().lines() {
                let _ = writeln!(out, "     {line}");
            }
        }
        let _ = writeln!(out, "  Finish gates:");
        for (agent, gate) in &sim.gates {
            if gate.is_open() {
                let _ = writeln!(out, "     {agent}: open");
            } else {
                let _ = writeln!(out, "     {agent}: BLOCKED ({} unresolved)", gate.blockers.len());
            }
        }
    }

    for w in &a.parse_warnings {
        let _ = writeln!(out, "warning: {w}");
    }
    out
}
