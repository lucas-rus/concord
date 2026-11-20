# RQ1 results: CooperBench gold patches (pilot, 2026-11-18)

- **Data:** all 560 feature pairs of the 9 Python repositories of CooperBench. That is 560 of 652 pairs; the Go/TypeScript/Rust pairs are pending language support.
- **Versions:** CooperBench commit `63b9d44`; Concord commit `7dbbbe5`.
- **Raw data:** [data/gold-pairs-2026-11-18.jsonl](data/gold-pairs-2026-11-18.jsonl).
- **Sanity check:** our textual merge results agree with CooperBench's `gold_conflict_report.json` on **560/560** pairs.

| repo | pairs | textual conflict | same symbol edited | file overlap, symbols disjoint | contract-coupled | breaking-coupled | breaking-coupled, textually clean | contract-coupled, textually clean |
|---|---|---|---|---|---|---|---|---|
| dottxt_ai_outlines | 79 | 61 (77%) | 32 (41%) | 47 (59%) | 31 (39%) | 9 (11%) | 1 (1%) | 1 (1%) |
| dspy | 55 | 38 (69%) | 36 (65%) | 16 (29%) | 39 (71%) | 15 (27%) | 4 (7%) | 10 (18%) |
| huggingface_datasets | 26 | 17 (65%) | 20 (77%) | 6 (23%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| openai_tiktoken | 45 | 45 (100%) | 45 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| pallets_click | 115 | 95 (83%) | 103 (90%) | 12 (10%) | 109 (95%) | 17 (15%) | 0 (0%) | 14 (12%) |
| pallets_jinja | 135 | 71 (53%) | 92 (68%) | 43 (32%) | 82 (61%) | 32 (24%) | 10 (7%) | 17 (13%) |
| pillow | 30 | 24 (80%) | 30 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| samuelcolvin_dirty_equals | 36 | 36 (100%) | 36 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| llama_index | 39 | 28 (72%) | 29 (74%) | 10 (26%) | 3 (8%) | 0 (0%) | 0 (0%) | 1 (3%) |
| **all** | 560 | 415 (74%) | 423 (76%) | 134 (24%) | 264 (47%) | 73 (13%) | 15 (3%) | 43 (8%) |

## Observations

1. **File-level coordination over-serialises.**
   - In **24%** of pairs (134/560), the two features share files but touch *disjoint* symbols in every shared file.
   - A file lock would serialise these needlessly. Symbol-level leases would let them run in parallel. Whether the textual merge is then clean depends on hunk adjacency, and a structured merge (e.g. Mergiraf) can resolve the rest.
2. **Most contract couplings are compatible.**
   - In **47%** of pairs, one feature changes the contract of a symbol that the other feature's code uses.
   - Only **13%** are *breaking* (73 pairs). The rest add optional parameters, `**kwargs` and the like, mostly to shared constructors such as `click.core.Context` and `jinja2.ext.InternationalizationExtension`.
   - Classifying compatibility cuts the number of pairs that would be gated by **72%**. This supports the design choice that compatible changes are informational only (RQ3/RQ4).
3. **Silent breaking couplings exist.**
   - **15 pairs (3%)** have a breaking contract coupling and still merge cleanly. These are candidates for "passes alone, fails together".
   - Examples:
     - `outlines` task 1706, features 1 and 6: a new required parameter `tensor_library_name` on `XGrammarLogitsProcessor`;
     - `dspy` task 8587, features 1 and 4: a new required parameter `is_last_chunk` on `StreamResponse`;
     - `jinja` task 1559, features 1 and 4: `_make_node` gains a required parameter and reorders positional ones.
   - **To verify:** running both features' tests on the clean merge (CooperBench Docker images) will confirm or refute each candidate.
4. **CooperBench under-represents cross-file coupling.**
   - Every pair shares at least one file (100%), because the feature pools are built around one PR's area.
   - As a result, no coupling across *disjoint* files occurs, unlike the thesis's running example. The evaluation therefore also needs tasks with cross-file dependencies: the constructed tier of *Passes Alone, Fails Together*, and Concord's own scenarios.

## Threats to validity

- Gold patches are not agent patches. Agents also overlap in ways gold patches do not (duplicated work, divergent architecture).
- `uses` resolution is a static over-approximation; precision and recall against an LSP oracle are still to be measured.
- Compatibility is decided syntactically and conservatively. For example, variable annotation changes and unknown decorators count as breaking.


## Update 2026-11-20: precision fixes and test-based validation

The first analysis over-reported couplings. Running real tests showed why, and three fixes followed:

1. **Instance attributes are not class uses.** `self.searchpath` used to resolve to the class `FileSystemLoader`, which made every reading method "depend on" the constructor. Only known members are resolved now.
2. **Call-site conformance.** A coupling is now counted as *violating* only if the consumer's calls really fail the new signature, checked by `compat::check_call` with the same rule the live finish gate uses. Previously any use of a symbol with a breaking change counted.
3. **Only the consumer's own call sites.** A call that already existed at the base is the producer's responsibility. Producers update such calls in their own branch, and git takes that change unless both sides edited the same lines, in which case it is a textual conflict. Example: jinja task 1559, features 1 and 5.

Re-run on all 560 pairs ([data/gold-pairs-2026-11-20.jsonl](data/gold-pairs-2026-11-20.jsonl)):

| repo | pairs | textual conflict | same symbol edited | file overlap, symbols disjoint | contract-coupled | breaking-coupled | call sites violate new contract | violating, textually clean | breaking-coupled, textually clean | contract-coupled, textually clean |
|---|---|---|---|---|---|---|---|---|---|---|
| dottxt_ai_outlines | 79 | 61 (77%) | 32 (41%) | 47 (59%) | 18 (23%) | 6 (8%) | 1 (1%) | 0 (0%) | 0 (0%) | 0 (0%) |
| dspy | 55 | 38 (69%) | 36 (65%) | 16 (29%) | 12 (22%) | 10 (18%) | 5 (9%) | 1 (2%) | 3 (5%) | 3 (5%) |
| huggingface_datasets | 26 | 17 (65%) | 20 (77%) | 6 (23%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| llama_index | 39 | 28 (72%) | 29 (74%) | 10 (26%) | 3 (8%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 1 (3%) |
| openai_tiktoken | 45 | 45 (100%) | 45 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| pallets_click | 115 | 95 (83%) | 103 (90%) | 12 (10%) | 13 (11%) | 3 (3%) | 1 (1%) | 0 (0%) | 0 (0%) | 4 (3%) |
| pallets_jinja | 135 | 71 (53%) | 92 (68%) | 43 (32%) | 19 (14%) | 2 (1%) | 0 (0%) | 0 (0%) | 1 (1%) | 1 (1%) |
| pillow | 30 | 24 (80%) | 30 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| samuelcolvin_dirty_equals | 36 | 36 (100%) | 36 (100%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) | 0 (0%) |
| **all** | 560 | 415 (74%) | 423 (76%) | 134 (24%) | 65 (12%) | 21 (4%) | 7 (1%) | 1 (0%) | 4 (1%) | 9 (2%) |

**Test-based validation** ([validate_pairs.py](validate_pairs.py)). For every textually clean pair, each feature's own tests run on its branch, then both test sets run on the merge:

- **jinja**, 64 clean pairs ([data](data/validation-jinja-2026-11-20.jsonl)):
  - 48 pairs could be evaluated; in the other 16, the two features' *test patches* conflict with each other.
  - No pair passes alone but fails together (0/48).
  - The first analysis flagged 10 of these pairs as breaking; all 10 pass together, i.e. false positives.
  - After the fixes, no jinja pair is flagged as a silent violation.
- **dspy:** its tests need a LiteLLM proxy fixture that only CooperBench's Docker images provide, so they could not run here. The single remaining silent violation in the whole dataset (dspy 8587, features 1 and 4: `StreamResponse` gains the required field `is_last_chunk`) is therefore **unverified**.

**What this means**
- Semantic interference between *gold* feature pairs is rare. This agrees with *Passes Alone, Fails Together*, which found almost none among merged human pull request pairs. Gold patches were written to be combined.
- The value of Concord on CooperBench should therefore be measured on **agent** patches, where the live runs show the failure mode clearly. The validation also gives the precision baseline for RQ4:
  - the naive coupling definition would have produced 15 spurious gate blocks on this data;
  - the conformance-based definition produces at most 1.
