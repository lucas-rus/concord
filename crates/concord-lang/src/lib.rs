//! # concord-lang
//!
//! Language front-ends that turn source files into Concord symbols (with
//! contract and body digests), import bindings and resolved references.
//! Supported: Python ([`python`]), TypeScript/JavaScript ([`typescript`]),
//! Rust ([`rust_lang`]), and Go ([`go_lang`]); [`index_file`] picks the
//! front-end by file extension.

pub mod cpp_lang;
pub mod go_lang;
pub mod python;
pub mod routes;
pub mod rust_lang;
pub mod schema;
pub mod typescript;

use concord_core::{CallArgs, Symbol, SymbolId, SymbolKind, UseSite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

#[derive(Debug, thiserror::Error)]
pub enum LangError {
    #[error("cannot load grammar: {0}")]
    Grammar(String),
    #[error("cannot parse {0}")]
    Parse(String),
}

/// `local` name bound in a file to a dotted `target` (module or module attribute).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportBinding {
    pub local: String,
    pub target: String,
    pub line: u32,
}

/// An unresolved reference: a name or attribute chain used inside `from`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawRef {
    pub from: SymbolId,
    pub chain: Vec<String>,
    pub line: u32,
    pub text: String,
    pub is_call: bool,
    /// Argument shape for calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<CallArgs>,
}

/// Everything Concord knows about one source file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileIndex {
    pub path: String,
    pub module: String,
    pub symbols: Vec<Symbol>,
    pub imports: Vec<ImportBinding>,
    /// Names this module re-exports (`local` = exported name, `*` for
    /// `export * from`; `target` = dotted symbol or module). TypeScript only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<ImportBinding>,
    pub refs: Vec<RawRef>,
    /// The parser recovered from syntax errors (results may be partial).
    pub has_errors: bool,
}

/// True if some front-end handles this path (by extension).
pub fn is_supported_path(path: &str) -> bool {
    python::is_python_path(path)
        || typescript::is_typescript_path(path)
        || rust_lang::is_rust_path(path)
        || go_lang::is_go_path(path)
        || cpp_lang::is_cpp_path(path)
        || schema::is_schema_path(path)
}

/// True if `path` follows standard test-file naming conventions across
/// Python, TypeScript/JavaScript, Rust, or Go.
pub fn is_test_path(path: &str) -> bool {
    let p = path.trim_start_matches("./");
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let Some(&file) = parts.last() else {
        return false;
    };
    if parts[..parts.len() - 1]
        .iter()
        .any(|&d| matches!(d, "tests" | "test" | "__tests__" | "spec" | "specs"))
    {
        return true;
    }
    file.starts_with("test_")
        || file.ends_with("_test.py")
        || file.ends_with("_test.go")
        || file.ends_with("_test.rs")
        || file.ends_with("_test.cpp")
        || file.ends_with("_test.cc")
        || file.contains(".test.")
        || file.contains(".spec.")
}

/// Index a file if its language is supported.
pub fn index_file(path: &str, source: &str) -> Option<Result<FileIndex, LangError>> {
    let mut res = if python::is_python_path(path) {
        Some(python::index_file(path, source))
    } else if typescript::is_typescript_path(path) {
        Some(typescript::index_file(path, source))
    } else if rust_lang::is_rust_path(path) {
        Some(rust_lang::index_file(path, source))
    } else if go_lang::is_go_path(path) {
        Some(go_lang::index_file(path, source))
    } else if cpp_lang::is_cpp_path(path) {
        Some(cpp_lang::index_file(path, source))
    } else if schema::is_schema_path(path) {
        Some(schema::index_file(path, source))
    } else {
        None
    };
    if let Some(Ok(ref mut fi)) = res {
        routes::enrich_http_contracts(fi, source);
    }
    res
}

/// A repository-wide index used to resolve references to symbols.
#[derive(Clone, Debug, Default)]
pub struct RepoIndex {
    files: BTreeMap<String, FileIndex>,
    by_id: HashMap<SymbolId, (String, usize)>,
    callables_by_name: HashMap<String, Vec<SymbolId>>,
    /// Module name -> its re-exports (exported name or `*`, target).
    reexports: HashMap<String, Vec<(String, String)>>,
    /// Distinct Go package module names present in the repository.
    go_modules: BTreeSet<String>,
}

impl RepoIndex {
    pub fn new(files: impl IntoIterator<Item = FileIndex>) -> Self {
        let mut idx = RepoIndex::default();
        for f in files {
            idx.files.insert(f.path.clone(), f);
        }
        idx.rebuild();
        idx
    }

    /// Replace (or add) a file and rebuild lookup tables.
    pub fn upsert(&mut self, file: FileIndex) {
        self.files.insert(file.path.clone(), file);
        self.rebuild();
    }

    pub fn remove(&mut self, path: &str) {
        self.files.remove(path);
        self.rebuild();
    }

    fn rebuild(&mut self) {
        self.by_id.clear();
        self.callables_by_name.clear();
        self.reexports.clear();
        self.go_modules.clear();
        for (path, f) in &self.files {
            if go_lang::is_go_path(path) && !f.module.is_empty() {
                self.go_modules.insert(f.module.clone());
            }
            for e in &f.exports {
                self.reexports
                    .entry(f.module.clone())
                    .or_default()
                    .push((e.local.clone(), e.target.clone()));
            }
            for (i, s) in f.symbols.iter().enumerate() {
                self.by_id.insert(s.id.clone(), (path.clone(), i));
                if matches!(s.kind, SymbolKind::Method | SymbolKind::Function) {
                    self.callables_by_name
                        .entry(s.id.name().to_owned())
                        .or_default()
                        .push(s.id.clone());
                }
            }
        }
    }

    fn resolve_go_import(&self, target: &str) -> Option<String> {
        let imp = target.strip_prefix("go:")?;
        self.go_modules
            .iter()
            .filter(|m| imp == m.as_str() || imp.ends_with(&format!(".{m}")))
            .max_by_key(|m| m.len())
            .cloned()
    }

    pub fn files(&self) -> impl Iterator<Item = &FileIndex> {
        self.files.values()
    }

    pub fn file(&self, path: &str) -> Option<&FileIndex> {
        self.files.get(path)
    }

    pub fn symbol(&self, id: &SymbolId) -> Option<&Symbol> {
        self.by_id.get(id).map(|(p, i)| &self.files[p].symbols[*i])
    }

    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.files.values().flat_map(|f| f.symbols.iter())
    }

    /// Symbols whose definitions overlap the given 1-based line range of a file
    /// (used to map `Read(file, offset, limit)` tool calls to symbol reads).
    pub fn symbols_in_lines(&self, path: &str, start: u32, end: u32) -> Vec<SymbolId> {
        self.files
            .get(path)
            .map(|f| {
                f.symbols
                    .iter()
                    .filter(|s| s.span.overlaps(start, end))
                    .map(|s| s.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Resolve a raw reference made in `file` to a known symbol.
    ///
    /// Strategy (sound-ish over-approximation, see docs/design.md):
    /// 1. `self.x` / `cls.x` (`this.x` in TypeScript) inside a method -> member
    ///    of the enclosing class;
    /// 2. names bound by imports (incl. `import a.b` chains and `*` imports);
    /// 3. module-level names of the same module;
    /// 4. for attribute *calls* on unknown receivers, a method/function with
    ///    that name if it is unique in the repository (in TypeScript, not for
    ///    names of built-in methods such as `map` or `get`).
    ///
    /// Lookups follow TypeScript re-exports (`export ... from`, barrels).
    pub fn resolve(&self, file: &FileIndex, r: &RawRef) -> Option<SymbolId> {
        let head = r.chain.first()?;
        let rest = &r.chain[1..];

        // Direct cross-language HTTP route reference (`http.<METHOD>.<path>`).
        if rest.is_empty() && head.starts_with("http.") {
            let sym = SymbolId::new(head);
            if self.by_id.contains_key(&sym) {
                return Some(sym);
            }
        }

        let is_py = python::is_python_path(&file.path);
        let is_ts = typescript::is_typescript_path(&file.path);
        let is_rs = rust_lang::is_rust_path(&file.path);
        let is_go = go_lang::is_go_path(&file.path);
        let is_cpp = cpp_lang::is_cpp_path(&file.path);
        let is_receiver = if is_ts || is_cpp {
            head == "this"
        } else if is_rs {
            head == "self" || head == "Self"
        } else {
            head == "self" || head == "cls"
        };
        if is_receiver
            && !rest.is_empty()
            && let Some(class) = self.enclosing_class(&r.from)
        {
            // `self.x.y`: only a known member counts. Reading an instance attribute
            // is not a use of the class (its constructor contract).
            return self.longest_prefix(class.as_str(), rest).filter(|s| s != &class);
        }

        // Later bindings shadow earlier ones.
        if let Some(b) = file.imports.iter().rev().find(|b| &b.local == head) {
            let target = self.resolve_go_import(&b.target);
            let eff = target.as_deref().unwrap_or(&b.target);
            if let Some(s) = self.longest_prefix(eff, rest) {
                return Some(s);
            }
            // Support tsconfig `@/* -> ./src/*` layout when `eff` has no `src.` prefix.
            if is_ts
                && !eff.starts_with(typescript::EXTERNAL_PREFIX)
                && !eff.starts_with("src.")
                && let Some(s) = self.longest_prefix(&format!("src.{eff}"), rest)
            {
                return Some(s);
            }
        }

        if let Some(s) = self.longest_prefix(&format!("{}.{}", file.module, head), rest)
            && !file.module.is_empty()
        {
            return Some(s);
        }
        if file.module.is_empty()
            && let Some(s) = self.longest_prefix(head, rest)
        {
            return Some(s);
        }

        for b in file.imports.iter().filter(|b| b.local == "*") {
            let target = self.resolve_go_import(&b.target);
            let eff = target.as_deref().unwrap_or(&b.target);
            if let Some(s) = self.longest_prefix(&format!("{eff}.{head}"), rest) {
                return Some(s);
            }
        }

        // A receiver bound by an import that did not resolve above is a
        // module outside the repository (`import re; re.match(...)`): never
        // guess a repository method for it.
        if file.imports.iter().any(|b| &b.local == head) {
            return None;
        }
        if r.is_call && !rest.is_empty() {
            let name = r.chain.last()?;
            if (is_py && python::is_builtin_method(name))
                || (is_ts && typescript::is_builtin_method(name))
                || (is_rs && rust_lang::is_builtin_method(name))
                || (is_go && go_lang::is_builtin_method(name))
            {
                return None;
            }
            if let Some(c) = self.callables_by_name.get(name)
                && c.len() == 1
            {
                return Some(c[0].clone());
            }
        }
        None
    }

    /// Resolved uses made from the given symbols (and, optionally, from the
    /// module-level code of the given files).
    pub fn uses_from(
        &self,
        from: &BTreeSet<SymbolId>,
        module_level_files: &BTreeSet<String>,
    ) -> BTreeMap<SymbolId, Vec<UseSite>> {
        let mut out: BTreeMap<SymbolId, Vec<UseSite>> = BTreeMap::new();
        for f in self.files.values() {
            let module_level = module_level_files.contains(&f.path);
            for r in &f.refs {
                let is_module_ref = r.from.as_str() == f.module;
                if !(from.contains(&r.from) || (module_level && is_module_ref)) {
                    continue;
                }
                let Some(target) = self.resolve(f, r) else { continue };
                if target == r.from {
                    continue;
                }
                let site = UseSite {
                    path: f.path.clone(),
                    line: r.line,
                    from: r.from.clone(),
                    text: r.text.clone(),
                    call: r.args.clone(),
                };
                let v = out.entry(target).or_default();
                if !v.contains(&site) {
                    v.push(site);
                }
            }
        }
        for v in out.values_mut() {
            v.sort();
        }
        out
    }

    fn enclosing_class(&self, from: &SymbolId) -> Option<SymbolId> {
        let mut cur = from.parent().map(SymbolId::new);
        while let Some(c) = cur {
            if self.symbol(&c).is_some_and(|s| s.kind == SymbolKind::Class) {
                return Some(c);
            }
            cur = c.parent().map(SymbolId::new);
        }
        if self.symbol(from).is_some_and(|s| s.kind == SymbolKind::Method) {
            return from.parent().map(SymbolId::new);
        }
        None
    }

    /// Test files transitively affected by `changed_symbols` or `changed_files`
    /// (`AffectedTests(G, W)` in the Concord verification loop).
    pub fn affected_tests(
        &self,
        changed_symbols: &BTreeSet<SymbolId>,
        changed_files: &BTreeSet<String>,
    ) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for f in changed_files {
            if is_test_path(f) {
                out.insert(f.clone());
            }
        }

        // Build reverse reference graph: target -> Vec<(caller_symbol, caller_path)>.
        let mut rev: HashMap<SymbolId, Vec<(SymbolId, String)>> = HashMap::new();
        for f in self.files.values() {
            for r in &f.refs {
                let Some(target) = self.resolve(f, r) else {
                    continue;
                };
                if target != r.from {
                    rev.entry(target).or_default().push((r.from.clone(), f.path.clone()));
                }
            }
        }

        let mut visited: BTreeSet<SymbolId> = BTreeSet::new();
        let mut queue: VecDeque<(SymbolId, usize)> = VecDeque::new();
        for s in changed_symbols {
            if visited.insert(s.clone()) {
                queue.push_back((s.clone(), 0));
            }
        }
        // If a file changed at module level without symbol edits, seed with its symbols.
        for path in changed_files {
            if let Some(f) = self.files.get(path) {
                for s in &f.symbols {
                    if visited.insert(s.id.clone()) {
                        queue.push_back((s.id.clone(), 0));
                    }
                }
            }
        }

        while let Some((sym, depth)) = queue.pop_front() {
            if let Some(callers) = rev.get(&sym) {
                for (caller, caller_path) in callers {
                    if is_test_path(caller_path) || caller.as_str().contains(".tests.") {
                        out.insert(caller_path.clone());
                    }
                    if depth < 4 && visited.insert(caller.clone()) {
                        queue.push_back((caller.clone(), depth + 1));
                    }
                }
            }
        }

        // Stem-based fallback (`src/cmark/inlines.py` -> `tests/test_inlines.py`).
        let stems: BTreeSet<String> = changed_files
            .iter()
            .filter_map(|p| p.rsplit('/').next())
            .map(|f| {
                f.split('.')
                    .next()
                    .unwrap_or(f)
                    .trim_start_matches("test_")
                    .trim_end_matches("_test")
                    .to_lowercase()
            })
            .filter(|s| !s.is_empty() && !matches!(s.as_str(), "init" | "__init__" | "index" | "mod" | "lib" | "main"))
            .collect();
        if !stems.is_empty() {
            for p in self.files.keys() {
                if is_test_path(p) {
                    let fname = p.rsplit('/').next().unwrap_or(p).to_lowercase();
                    if stems.iter().any(|st| fname.contains(st)) {
                        out.insert(p.clone());
                    }
                }
            }
        }

        out
    }

    /// Longest known symbol among `base`, `base.rest[0]`, `base.rest[0].rest[1]`, ...
    fn longest_prefix(&self, base: &str, rest: &[String]) -> Option<SymbolId> {
        let mut candidates = vec![base.to_owned()];
        let mut cur = base.to_owned();
        for part in rest {
            cur.push('.');
            cur.push_str(part);
            candidates.push(cur.clone());
        }
        let direct = candidates
            .into_iter()
            .rev()
            .map(SymbolId::new)
            .find(|c| self.by_id.contains_key(c));
        if direct.is_some() || self.reexports.is_empty() {
            return direct;
        }
        self.via_reexports(base, rest, 0)
    }

    /// Follow re-exports: `m.x.rest` where module `m` re-exports `x` (or `*`)
    /// from a target. Bounded depth guards against export cycles.
    fn via_reexports(&self, base: &str, rest: &[String], depth: usize) -> Option<SymbolId> {
        if depth > 8 {
            return None;
        }
        let mut segs: Vec<String> = base.split('.').filter(|s| !s.is_empty()).map(str::to_owned).collect();
        segs.extend(rest.iter().cloned());
        for i in (0..segs.len()).rev() {
            let Some(exports) = self.reexports.get(&segs[..i].join(".")) else {
                continue;
            };
            let name = &segs[i];
            let tail = &segs[i + 1..];
            for (exported, target) in exports {
                let base = if exported == name {
                    target.clone()
                } else if exported == "*" {
                    format!("{target}.{name}")
                } else {
                    continue;
                };
                let found = self
                    .lookup(&base, tail)
                    .or_else(|| self.via_reexports(&base, tail, depth + 1));
                if found.is_some() {
                    return found;
                }
            }
        }
        None
    }

    /// Longest known symbol among `base`, `base.rest[0]`, ... (no re-exports).
    fn lookup(&self, base: &str, rest: &[String]) -> Option<SymbolId> {
        let mut cur = base.to_owned();
        let mut best = self
            .by_id
            .contains_key(&SymbolId::new(cur.clone()))
            .then(|| SymbolId::new(cur.clone()));
        for part in rest {
            cur.push('.');
            cur.push_str(part);
            let id = SymbolId::new(cur.clone());
            if self.by_id.contains_key(&id) {
                best = Some(id);
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(files: &[(&str, &str)]) -> RepoIndex {
        RepoIndex::new(files.iter().map(|(p, s)| python::index_file(p, s).unwrap()))
    }

    #[test]
    fn resolves_cross_file_calls() {
        let r = idx(&[
            ("notify/__init__.py", ""),
            ("notify/email.py", "def send_email(to, body):\n    pass\n"),
            (
                "auth/reset.py",
                "from notify.email import send_email\n\ndef request_reset(user):\n    send_email(user.email, 'x')\n",
            ),
            (
                "app.py",
                "import notify.email\n\ndef main():\n    notify.email.send_email('a', 'b')\n",
            ),
        ]);
        let from: BTreeSet<SymbolId> = ["auth.reset.request_reset".into(), "app.main".into()].into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        let sites = &uses[&SymbolId::new("notify.email.send_email")];
        assert_eq!(sites.len(), 2);
        assert!(sites.iter().any(|s| s.path == "auth/reset.py" && s.line == 4));
        assert!(sites.iter().any(|s| s.path == "app.py"));
    }

    #[test]
    fn resolves_self_methods_and_unique_method_names() {
        let r = idx(&[
            (
                "shop/cart.py",
                "class Cart:\n    def add(self, x):\n        self.check(x)\n    def check(self, x):\n        pass\n",
            ),
            ("shop/api.py", "def handler(cart):\n    cart.add(1)\n"),
        ]);
        let from: BTreeSet<SymbolId> = ["shop.cart.Cart.add".into(), "shop.api.handler".into()].into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        assert!(uses.contains_key(&SymbolId::new("shop.cart.Cart.check")));
        assert!(uses.contains_key(&SymbolId::new("shop.cart.Cart.add")));
    }

    #[test]
    fn calls_on_external_modules_are_not_unique_name_matches() {
        let ix = idx(&[
            (
                "p/parser.py",
                "class InlineParser:\n    def match(self, s):\n        return s\n",
            ),
            (
                "p/blocks.py",
                "import re\n\ndef f(start, s):\n    return re.match(start, s)\n\ndef g(p, s):\n    return p.match(s)\n",
            ),
        ]);
        let from: BTreeSet<SymbolId> = [SymbolId::new("p.blocks.f"), SymbolId::new("p.blocks.g")].into();
        let uses = ix.uses_from(&from, &BTreeSet::new());
        let sites = &uses[&SymbolId::new("p.parser.InlineParser.match")];
        // Only `p.match(s)` (unknown receiver) is a candidate, not `re.match`.
        assert_eq!(sites.len(), 1, "{sites:?}");
        assert!(sites[0].text.contains("p.match"));
    }

    #[test]
    fn instance_attributes_are_not_uses_of_the_class() {
        let r = idx(&[(
            "m.py",
            "class L:\n    def __init__(self, p):\n        self.p = p\n    def f(self):\n        return self.p\n",
        )]);
        let uses = r.uses_from(&["m.L.f".into()].into(), &BTreeSet::new());
        assert!(!uses.contains_key(&SymbolId::new("m.L")), "{uses:?}");
    }

    #[test]
    fn module_level_imports_count_as_uses() {
        let r = idx(&[
            ("notify/email.py", "def send_email(to, body):\n    pass\n"),
            ("auth/reset.py", "from notify.email import send_email\n"),
        ]);
        let uses = r.uses_from(&BTreeSet::new(), &["auth/reset.py".to_string()].into());
        assert!(uses.contains_key(&SymbolId::new("notify.email.send_email")));
    }

    #[test]
    fn maps_line_ranges_to_symbols() {
        let r = idx(&[("m.py", "def a():\n    pass\n\ndef b():\n    pass\n")]);
        assert_eq!(r.symbols_in_lines("m.py", 4, 5), vec![SymbolId::new("m.b")]);
        assert_eq!(r.symbols_in_lines("m.py", 1, 5).len(), 2);
    }

    #[test]
    fn resolves_rust_cross_file_calls_and_methods() {
        let files = [
            (
                "src/notify/email.rs",
                "pub struct Mailer { pub host: String }\nimpl Mailer {\n    pub fn send(&self, to: &str, body: &str) -> bool { self.validate(to) && !body.is_empty() }\n    fn validate(&self, to: &str) -> bool { !to.is_empty() }\n}\npub fn send_email(to: &str, body: &str) -> bool { !to.is_empty() && !body.is_empty() }\n",
            ),
            (
                "src/auth/reset.rs",
                "use crate::notify::email::{Mailer, send_email};\npub fn reset_pw(addr: &str) -> bool {\n    let m = Mailer { host: \"smtp\".into() };\n    m.send(addr, \"tok\") && send_email(addr, \"tok\")\n}\n",
            ),
        ];
        let r = RepoIndex::new(files.iter().map(|(p, s)| crate::index_file(p, s).unwrap().unwrap()));
        let from: BTreeSet<SymbolId> = [
            SymbolId::new("auth.reset.reset_pw"),
            SymbolId::new("notify.email.Mailer.send"),
        ]
        .into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        assert!(uses.contains_key(&SymbolId::new("notify.email.send_email")), "{uses:?}");
        assert!(uses.contains_key(&SymbolId::new("notify.email.Mailer")), "{uses:?}");
        assert!(
            uses.contains_key(&SymbolId::new("notify.email.Mailer.send")),
            "{uses:?}"
        );
        assert!(
            uses.contains_key(&SymbolId::new("notify.email.Mailer.validate")),
            "{uses:?}"
        );
    }

    #[test]
    fn resolves_go_cross_package_calls_and_receiver_methods() {
        let files = [
            (
                "pkg/notify/email.go",
                "package notify\n\ntype Client struct{}\nfunc (c *Client) Send(to string, body string) error { return c.check(to) }\nfunc (c *Client) check(to string) error { return nil }\nfunc SendEmail(to string, body string) error { return nil }\n",
            ),
            (
                "pkg/auth/reset.go",
                "package auth\n\nimport (\n\t\"fmt\"\n\t\"github.com/acme/app/pkg/notify\"\n)\n\nfunc Reset(c *notify.Client, addr string) error {\n\tfmt.Println(addr)\n\t_ = c.Send(addr, \"tok\")\n\treturn notify.SendEmail(addr, \"tok\")\n}\n",
            ),
        ];
        let r = RepoIndex::new(files.iter().map(|(p, s)| crate::index_file(p, s).unwrap().unwrap()));
        let from: BTreeSet<SymbolId> =
            [SymbolId::new("pkg.auth.Reset"), SymbolId::new("pkg.notify.Client.Send")].into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        assert!(uses.contains_key(&SymbolId::new("pkg.notify.SendEmail")), "{uses:?}");
        assert!(uses.contains_key(&SymbolId::new("pkg.notify.Client.Send")), "{uses:?}");
        assert!(uses.contains_key(&SymbolId::new("pkg.notify.Client.check")), "{uses:?}");
    }

    #[test]
    fn affected_tests_traces_transitive_callers() {
        let r = idx(&[
            ("notify/email.py", "def send_email(to, body):\n    return True\n"),
            (
                "auth/reset.py",
                "from notify.email import send_email\ndef request_reset(u):\n    return send_email(u, 'x')\n",
            ),
            (
                "tests/test_reset.py",
                "from auth.reset import request_reset\ndef test_pw():\n    assert request_reset('a@b')\n",
            ),
            ("tests/test_unrelated.py", "def test_other():\n    assert 1 + 1 == 2\n"),
        ]);
        let changed_syms: BTreeSet<SymbolId> = [SymbolId::new("notify.email.send_email")].into();
        let changed_files: BTreeSet<String> = ["notify/email.py".to_owned()].into();
        let affected = r.affected_tests(&changed_syms, &changed_files);
        assert!(affected.contains("tests/test_reset.py"), "{affected:?}");
        assert!(!affected.contains("tests/test_unrelated.py"), "{affected:?}");
    }
}
