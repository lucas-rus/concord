# Related work and prior art

A living index of the research and tools Concord builds on. Numbers are quoted from the linked primary sources. Many 2026 items are arXiv preprints and are not yet peer-reviewed.

## Evidence that parallel agents interfere

| Source | Key finding |
|---|---|
| [AgenticFlict](https://arxiv.org/abs/2604.03551) (Ogenrwot & Businge, AIware 2026) | Replayed 107K+ agentic PRs from 59K+ repos: **27.67%** have textual merge conflicts; 336K+ conflict regions. Textual only. |
| [AI Agent PRs on GitHub](https://arxiv.org/abs/2607.04697) (Xu, Subramanian, Karthik 2026) | 33,596 agent PRs: 40.2% of repos have concurrently open agent PR pairs; conflict rate 19.8% (same agent) vs. 41.7% (different agents); 84.4% of conflicted files are source code. |
| [How AI Coding Agents Resolve Merge Conflicts](https://cbsoft.sbc.org.br/2026/data/papers/sbes/How%20AI%20Coding%20Agents%20Resolve%20Merge%20Conflicts%20An%20Empirical%20Study.pdf) (Campos Junior & Murta, SBES 2026) | Humans author **96.1%** of conflicting merge commits in AI-authored PRs; agents' self-resolution rates vary ~60× by vendor. |
| [CooperBench](https://arxiv.org/abs/2601.13295) (Khatua et al., Stanford/SAP 2026) | 652 two-feature tasks, 12 repos, 4 languages. The "curse of coordination": cooperation retains 0.59 of solo performance; success 68.6% → 46.5% → 30.0% with 2/3/4 agents. Free-form messaging does not significantly improve success. Failure symptoms: work overlap 33.2%, divergent architecture 29.7%. [Harness](https://github.com/cooperbench/CooperBench). |
| [Passes Alone, Fails Together](https://arxiv.org/abs/2609.25396) (Xia, Wu, Park 2026) | Semantic interference after a clean merge: 97% of constructed Django interface-change tasks. **A message describing the concurrent change recovers 82%.** Rare in 417 mined merged PR pairs. Benchmark only, no mitigation. |
| [Agentic SE roadmap](https://arxiv.org/abs/2509.06216) (Hassan et al. 2025/2026 v4) | SE 3.0 and "Structured Agentic Software Engineering" (SASE): introduces **Merge-Readiness Packs (MRPs)** (5 pillars: Functional Completeness, Sound Verification, SE Hygiene, Rationale, Auditability) and Consultation Request Packs (CRPs). |
| [AIDev](https://arxiv.org/abs/2507.15003) / [AIDev 2026](https://arxiv.org/abs/2602.09185) (Li, Zhang, Hassan 2025/2026) | 456K → 932,791 autonomous agent PRs across Claude Code, Codex, Devin, Copilot, and Cursor; TypeScript (26.4%), Python (20.1%), Go (8.4%), and Rust (5.7%) dominate real agent workloads, with heavy-tailed p95 > 60 min latency. |
| [MCP Smells](https://arxiv.org/abs/2602.14878) & [MCP Security](https://doi.org/10.1145/3814959) (Hasan et al. 2026) | Demonstrates that verbose/smelly MCP tool descriptions and full-file payloads degrade agent accuracy and token efficiency, motivating Concord's contract-only `concord_symbols` and `concord_diff_contracts` tools. |
| Automated Multi-Agent Failure Attribution (Zhang et al., ICML 2025) | Formalises attributing multi-agent failures to minimal culprit subsets, grounding Concord's $O(\log k)$ peer bisection in `fails-together` and `smart_merge`. |

## Coordination mechanisms for concurrent coding agents (research)

| Work | Mechanism | Limitation relevant to Concord |
|---|---|---|
| [CodeCRDT](https://arxiv.org/abs/2510.18893) (2025) | Character-level CRDT shared state; agents observe each other | 5–10% semantic conflicts remain; −39% to +21% speed |
| [AgentRoom](https://arxiv.org/abs/2608.23740) (2026) | File-level claims + CRDT filesystem, as MCP tools | File granularity; LLM-judge scoring; no dependency graph, symbol locks or speculative merge |
| [grite / Before the Pull Request](https://arxiv.org/abs/2606.19616) (2026) | Signed claim log stored in git | Task-level claims; duplicate work 78% → 0% |
| [ATM](https://arxiv.org/abs/2607.00041), [Claim Plane](https://arxiv.org/abs/2608.00947) (2026), [code](https://github.com/SkeinRank/claim-plane) | Planner-declared change intents, pre-write admission | On 30 CooperBench pairs: pair pass 23.3% → 50.0%, but **96.7% of executions serialised**; dynamic admission failed closed in 46/90 runs because declarations under-covered the agents' real footprint |
| [CoAgent](https://arxiv.org/abs/2606.15376) (2026) | Monotonic trajectory pre-order, speculative writes, notifications | Objects are files/dirs; footprints must be **declared** per tool |
| [S-Bus](https://arxiv.org/abs/2605.17076) (2026) | Read sets reconstructed from observed HTTP traffic | Generic shared state, not code; agents' self-reports overstate usage by 32–49% |
| [Token Coherence](https://arxiv.org/abs/2603.15183) (2026) | MESI invalidation for shared artifacts | Simulation only, not code |
| [Co-Coder](https://arxiv.org/abs/2606.00953) (2026) | Static dependency graph → cohesive partitions before execution | Static and up front; dependencies that appear at run time are out of scope |

## Tools (practice)

| Tool | What it does | What Concord borrows / how it differs |
|---|---|---|
| [Claude Code agent teams](https://code.claude.com/docs/en/agent-teams) | Lead + teammates, shared task list, mailboxes | Docs: "Break the work so each teammate owns a different set of files." No code-level coordination |
| [Claude Code hooks](https://code.claude.com/docs/en/hooks) | Pre/PostToolUse, PostToolBatch, Stop, TaskCompleted; `additionalContext`; `http` hooks | Concord's integration surface |
| [Clash](https://github.com/clash-sh/clash) (Rust, MIT) | Pairwise worktree `merge-tree` (gix), PreToolUse warning | File-level, textual only |
| [Grove](https://github.com/NathanDrake2406/grove) (Rust, Apache-2.0) | Overlap scores on file/hunk/symbol/import/schema layers, advisory hook | Closest prior art; passive, no contract/body split, no reads, no gate, no evaluation |
| [Aura](https://github.com/Naridon-Inc/aura) (Apache-2.0) | AST merge, declared symbol claims, radar | Declared claims, no cross-file signature conformance check or published evaluation |
| [Weave](https://github.com/Ataraxy-Labs/weave) (Rust, 2026) | Tree-sitter entity-level git merge driver + CRDT entity claims over MCP | Declared MCP entity claims; merges same-file entities cleanly, whereas Concord infers footprints automatically, splits contract vs. body, verifies cross-file call conformance, and gates stale consumers |
| [Cork](https://github.com/Cork-Technology/cork-cli) (2026) | Collaborative VCS CLI for parallel AI agents | Replaces plain Git workflow; Concord works directly on standard Git worktrees and branches |
| [Gas Town](https://github.com/gastownhall/gastown) (Go, MIT) | Agent factory; Refinery = Bors-style bisecting merge queue | Model for Concord's integration queue; detects at merge time |
| [merge-semantics](https://github.com/shouvikkkk/merge-semantics) | Post-merge semantic checker | Finding taxonomy (removed-still-referenced, rename-vs-old-caller) |
| Worktree managers ([Claude Squad](https://github.com/smtg-ai/claude-squad), [Vibe Kanban](https://github.com/BloopAI/vibe-kanban)) | Parallel sessions in worktrees | Isolation only |
| [Serena](https://github.com/oraios/serena) | LSP-backed symbol tools for agents | SolidLSP design for driving ty/pyright/pyrefly (MIT part only) |
| [Griffe](https://github.com/mkdocstrings/griffe) (ISC), [cargo-semver-checks](https://github.com/obi1kenobi/cargo-semver-checks) | API breaking-change detection between releases | Inspiration for `compat::classify`; Griffe is a possible oracle for validating it |
| [aider repo map](https://github.com/Aider-AI/aider) | tree-sitter tags + PageRank | Ranking which use sites to show in notices |

## Foundations

- **Proactive conflict detection:**
  - [Palantír](https://doi.org/10.1109/TSE.2011.64) (Sarma et al., TSE 2012): workspace awareness, direct and indirect conflicts.
  - [Crystal](https://doi.org/10.1145/2025113.2025139) (Brun et al., ESEC/FSE 2011): speculative merge plus build and test.
  - [WeCode](https://doi.org/10.1109/ICSE.2012.6227180) (Guimarães & Silva, ICSE 2012).
- **Structured and semantic merge:**
  - [Semistructured merge](https://doi.org/10.1145/2025113.2025141) (Apel et al. 2011).
  - [Mergiraf](https://mergiraf.org).
  - [SAM: semantic conflicts via unit tests](https://doi.org/10.1016/j.jss.2024.112070) (Da Silva et al., JSS 2024).
- **Concurrency control and coherence:**
  - [Optimistic concurrency control](https://doi.org/10.1145/319566.319567) (Kung & Robinson 1981).
  - [Leases](https://doi.org/10.1145/74850.74870) (Gray & Cheriton 1989).
  - [MESI](https://doi.org/10.1145/800015.808204) (Papamarcos & Patel 1984).
  - [CRDTs](https://doi.org/10.1007/978-3-642-24550-3_29) (Shapiro et al. 2011).
- **Agents and benchmarks:**
  - [SWE-bench](https://arxiv.org/abs/2310.06770).
  - [SWE-agent](https://arxiv.org/abs/2405.15793).
  - [OpenHands](https://arxiv.org/abs/2407.16741) ([software-agent-sdk](https://github.com/OpenHands/software-agent-sdk)).
  - [Agentless](https://arxiv.org/abs/2407.01489).
  - [MetaGPT](https://arxiv.org/abs/2308.00352).
  - [ChatDev](https://arxiv.org/abs/2307.07924).

The full bibliography of the thesis proposal, with direct PDF links, repositories and a note on how each source helps Concord, is in [`docs/bibliography/references.txt`](../bibliography/references.txt) (BibTeX: [`references.bib`](../bibliography/references.bib)).
