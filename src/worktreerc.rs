use std::path::{Path, PathBuf};

use crate::config::{
    ConflictPolicy, EffectiveConfig, FileRule, FileRuleKind, MatchMode, NonEmptyArgv, RelativePath,
    SourceRoot, Task, TaskPhase,
};

#[derive(Debug, Clone, serde::Serialize)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportResult {
    pub config: EffectiveConfig,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(serialize_with = "crate::domain::serialize_path")]
    pub source: PathBuf,
}

#[derive(Debug, Clone)]
enum Value {
    String(String),
    Array(Vec<String>),
    Decimal(String),
}

type Assignment = (String, Value, usize, usize);
type ParseResult = Result<(Vec<Assignment>, Vec<Diagnostic>), Vec<Diagnostic>>;

struct Parser {
    input: Vec<char>,
    index: usize,
    line: usize,
    column: usize,
    recovery_quote: Option<char>,
}

impl Parser {
    fn new(source: &str) -> Self {
        Self {
            input: source.chars().collect(),
            index: 0,
            line: 1,
            column: 1,
            recovery_quote: None,
        }
    }

    fn parse(mut self) -> ParseResult {
        let mut assignments = Vec::new();
        let mut diagnostics = Vec::new();
        while self.skip_space_and_comments() {
            let line = self.line;
            let column = self.column;
            let key = match self.identifier() {
                Ok(key) => key,
                Err(error) => return Err(vec![error]),
            };
            self.skip_horizontal_space();
            if self.bump() != Some('=') {
                return Err(vec![self.error("expected '='")]);
            }
            self.skip_horizontal_space();
            let array_assignment = self.peek() == Some('(');
            match self.value() {
                Ok(value) => assignments.push((key, value, line, column)),
                Err(error) => {
                    if error.message.starts_with("structural") {
                        return Err(vec![error]);
                    }
                    diagnostics.push(error);
                    let quote = self.recovery_quote.take();
                    if array_assignment {
                        if !self.recover_array(quote) {
                            return Err(vec![self.error("structural: unterminated array")]);
                        }
                    } else {
                        self.skip_bad_value();
                    }
                }
            }
            self.skip_horizontal_space();
            if matches!(self.peek(), Some('#')) {
                self.skip_comment();
            }
            if let Some(char) = self.peek() {
                if char != '\n' {
                    return Err(vec![self.error("unexpected trailing token")]);
                }
            }
        }
        Ok((assignments, diagnostics))
    }

    fn identifier(&mut self) -> Result<String, Diagnostic> {
        let mut value = String::new();
        while let Some(char) = self.peek() {
            if char.is_ascii_alphanumeric() || char == '_' {
                value.push(char);
                self.bump();
            } else {
                break;
            }
        }
        if value.is_empty() {
            Err(self.error("structural: expected identifier"))
        } else {
            Ok(value)
        }
    }

    fn value(&mut self) -> Result<Value, Diagnostic> {
        match self.peek() {
            Some('\'') | Some('"') => self.quoted().map(Value::String),
            Some('(') => self.array().map(Value::Array),
            Some(char) if char.is_ascii_digit() => {
                let start = self.index;
                while self
                    .peek()
                    .is_some_and(|char| !char.is_whitespace() && char != '#')
                {
                    self.bump();
                }
                Ok(Value::Decimal(
                    self.input[start..self.index].iter().collect(),
                ))
            }
            Some('[') => {
                Err(self.error("structural: expected Bash array '('; '[' arrays are not accepted"))
            }
            Some(_) => Err(self.error("unquoted words and shell syntax are not allowed")),
            None => Err(self.error("structural: expected value")),
        }
    }

    fn quoted(&mut self) -> Result<String, Diagnostic> {
        let Some(quote) = self.bump() else {
            return Err(self.error("structural: expected quote"));
        };
        let mut value = String::new();
        loop {
            match self.bump() {
                Some(char) if char == quote => return Ok(value),
                Some('\\') => {
                    self.recovery_quote = Some(quote);
                    return Err(self.error("backslash escapes are not accepted"));
                }
                Some('\0') => {
                    self.recovery_quote = Some(quote);
                    return Err(self.error("NUL is not accepted"));
                }
                Some(char) if "$`;&|<>".contains(char) => {
                    self.recovery_quote = Some(quote);
                    return Err(self.error("shell syntax is not accepted in literals"));
                }
                Some(char) => value.push(char),
                None => return Err(self.error("structural: unterminated quote")),
            }
        }
    }

    fn array(&mut self) -> Result<Vec<String>, Diagnostic> {
        self.bump();
        let mut values = Vec::new();
        loop {
            self.skip_space_and_comments();
            match self.peek() {
                Some(')') => {
                    self.bump();
                    return Ok(values);
                }
                Some('\'') | Some('"') => values.push(self.quoted()?),
                Some(char) if is_safe_array_char(char) => {
                    let mut value = String::new();
                    while let Some(char) = self.peek() {
                        if char.is_whitespace() || char == '#' || char == ')' {
                            break;
                        }
                        if !is_safe_array_char(char) {
                            return Err(self.error("array entry contains shell syntax"));
                        }
                        value.push(char);
                        self.bump();
                    }
                    values.push(value);
                }
                Some(_) => {
                    return Err(self.error("array entries must be quoted or safe literal paths"));
                }
                None => return Err(self.error("structural: unterminated array")),
            }
        }
    }

    fn skip_bad_value(&mut self) {
        while let Some(char) = self.peek() {
            if char == '\n' {
                return;
            }
            self.bump();
        }
    }

    fn recover_array(&mut self, active_quote: Option<char>) -> bool {
        let mut depth = 1usize;
        if let Some(quote) = active_quote {
            while let Some(char) = self.bump() {
                if char == quote {
                    break;
                }
            }
            if self.peek().is_none() {
                return false;
            }
        }
        while let Some(char) = self.bump() {
            match char {
                '\'' | '"' => {
                    let quote = char;
                    loop {
                        match self.bump() {
                            Some(value) if value == quote => break,
                            Some(_) => {}
                            None => return false,
                        }
                    }
                }
                '#' => {
                    while let Some(value) = self.bump() {
                        if value == '\n' {
                            break;
                        }
                    }
                }
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return true;
                    }
                }
                _ => {}
            }
        }
        false
    }

    fn skip_space_and_comments(&mut self) -> bool {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() == Some('#') {
                self.skip_comment();
            } else {
                return self.peek().is_some();
            }
        }
    }

    fn skip_horizontal_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.bump();
        }
    }

    fn skip_comment(&mut self) {
        while let Some(char) = self.bump() {
            if char == '\n' {
                break;
            }
        }
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.index).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.index += 1;
        if value == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(value)
    }

    fn error(&self, message: &str) -> Diagnostic {
        Diagnostic {
            line: self.line,
            column: self.column,
            message: message.into(),
        }
    }
}

fn is_safe_array_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || "_./-*:@+".contains(char)
}

pub fn import_source(source: &str, path: &Path) -> Result<ImportResult, Vec<Diagnostic>> {
    if source.contains('\0') {
        return Err(vec![Diagnostic {
            line: 1,
            column: 1,
            message: "NUL is not accepted".into(),
        }]);
    }
    let (assignments, mut diagnostics) = Parser::new(source).parse()?;
    let mut config = EffectiveConfig::empty();
    add_implicit_env_rule(&mut config, &mut diagnostics);
    let mut seen = std::collections::BTreeSet::new();
    let mut symlink_index = 0;
    let mut relink_index = 0;
    for (key, value, line, column) in assignments {
        if !seen.insert(key.clone()) {
            diagnostics.push(Diagnostic {
                line,
                column,
                message: format!("duplicate assignment: {key}"),
            });
            continue;
        }
        match key.as_str() {
            "SYMLINK_PATHS" => map_paths(
                &mut config,
                &mut diagnostics,
                value,
                true,
                &mut symlink_index,
                line,
                column,
            ),
            "RELINK_PATHS" => map_paths(
                &mut config,
                &mut diagnostics,
                value,
                false,
                &mut relink_index,
                line,
                column,
            ),
            "WORKTREE_COPY_CODEGRAPH" => {
                map_codegraph(&mut config, &mut diagnostics, value, line, column)
            }
            "WORKTREE_SLUG_MAX" => map_slug(&mut config, &mut diagnostics, value, line, column),
            "WORKTREE_ROOT" => map_root(&mut config, &mut diagnostics, value, line, column),
            "WORKTREE_DIR_PREFIX" => map_prefix(&mut config, &mut diagnostics, value, line, column),
            "WORKTREE_INSTALL_CMD" => map_task(
                &mut config,
                &mut diagnostics,
                value,
                "install",
                line,
                column,
            ),
            "WORKTREE_BUILD_CMD" => {
                map_task(&mut config, &mut diagnostics, value, "build", line, column)
            }
            _ => diagnostics.push(Diagnostic {
                line,
                column,
                message: format!("unknown variable {key}"),
            }),
        }
    }
    Ok(ImportResult {
        config,
        diagnostics,
        source: path.to_owned(),
    })
}

fn add_implicit_env_rule(config: &mut EffectiveConfig, diagnostics: &mut Vec<Diagnostic>) {
    let source = RelativePath::new("**/.env*".into());
    let destination = RelativePath::new(".".into());
    let exclude = RelativePath::new(".env.example".into());
    if let (Ok(source), Ok(destination), Ok(exclude)) = (source, destination, exclude) {
        config.file_rules.insert(
            "legacy_env_copy".into(),
            FileRule {
                match_mode: MatchMode::Glob,
                kind: FileRuleKind::Copy,
                source,
                destination,
                source_root: SourceRoot::PrimaryWorktree,
                on_conflict: ConflictPolicy::Fail,
                ignored_only: true,
                excludes: vec![exclude],
                enabled: false,
                sensitive: true,
                confirm: true,
            },
        );
        diagnostics.push(Diagnostic {
            line: 1,
            column: 1,
            message: "implicit .env copy is disabled; enable it explicitly after review".into(),
        });
    }
}

fn map_paths(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    symlink: bool,
    index: &mut usize,
    line: usize,
    column: usize,
) {
    let Value::Array(values) = value else {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "expected Bash array".into(),
        });
        return;
    };
    for path in values {
        let name = format!(
            "legacy_{}_{}",
            if symlink { "symlink" } else { "relink" },
            *index
        );
        *index += 1;
        let source = match RelativePath::new(path.clone()) {
            Ok(path) => path,
            Err(message) => {
                diagnostics.push(Diagnostic {
                    line,
                    column,
                    message,
                });
                continue;
            }
        };
        let destination = match RelativePath::new(path) {
            Ok(path) => path,
            Err(message) => {
                diagnostics.push(Diagnostic {
                    line,
                    column,
                    message,
                });
                continue;
            }
        };
        config.file_rules.insert(
            name,
            FileRule {
                match_mode: MatchMode::Path,
                kind: if symlink {
                    FileRuleKind::Symlink
                } else {
                    FileRuleKind::Relink
                },
                source,
                destination,
                source_root: SourceRoot::PrimaryWorktree,
                on_conflict: if symlink {
                    ConflictPolicy::Fail
                } else {
                    ConflictPolicy::ReplaceSymlinkOnly
                },
                ignored_only: false,
                excludes: Vec::new(),
                enabled: false,
                sensitive: true,
                confirm: true,
            },
        );
    }
}

fn map_codegraph(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    line: usize,
    column: usize,
) {
    if !matches!(value, Value::Decimal(ref number) if number == "0" || number == "1") {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "WORKTREE_COPY_CODEGRAPH must be strict 0 or 1".into(),
        });
        return;
    }
    if matches!(value, Value::Decimal(number) if number == "0") {
        return;
    }
    let rule = |path: &str| RelativePath::new(path.into());
    let (Ok(source), Ok(destination), Ok(a), Ok(b), Ok(c)) = (
        rule(".codegraph"),
        rule(".codegraph"),
        rule("daemon.pid"),
        rule("*.sock"),
        rule("*.log"),
    ) else {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "internal codegraph mapping error".into(),
        });
        return;
    };
    config.file_rules.insert(
        "legacy_codegraph".into(),
        FileRule {
            match_mode: MatchMode::Path,
            kind: FileRuleKind::CopyTree,
            source,
            destination,
            source_root: SourceRoot::PrimaryWorktree,
            on_conflict: ConflictPolicy::Fail,
            ignored_only: false,
            excludes: vec![a, b, c],
            enabled: false,
            sensitive: true,
            confirm: true,
        },
    );
    diagnostics.push(Diagnostic {
        line,
        column,
        message: "codegraph copy is disabled pending explicit trust".into(),
    });
}

fn scalar_string(value: Value) -> Option<String> {
    if let Value::String(value) = value {
        Some(value)
    } else {
        None
    }
}

fn map_slug(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    line: usize,
    column: usize,
) {
    match value {
        Value::Decimal(value) => match value.parse::<usize>() {
            Ok(value) if value >= 8 => config.create.slug_max_bytes = value,
            _ => diagnostics.push(Diagnostic {
                line,
                column,
                message: "slug maximum must be decimal and at least 8".into(),
            }),
        },
        _ => diagnostics.push(Diagnostic {
            line,
            column,
            message: "slug maximum must be an unquoted decimal".into(),
        }),
    }
}

fn map_root(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    line: usize,
    column: usize,
) {
    let Some(value) = scalar_string(value) else {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "WORKTREE_ROOT must be quoted".into(),
        });
        return;
    };
    if value.is_empty() || value.contains('\0') {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "WORKTREE_ROOT must be nonempty and contain no NUL".into(),
        });
    } else {
        config.create.worktree_root = Some(value);
    }
}

fn map_prefix(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    line: usize,
    column: usize,
) {
    let Some(value) = scalar_string(value) else {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "WORKTREE_DIR_PREFIX must be quoted".into(),
        });
        return;
    };
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
    {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "WORKTREE_DIR_PREFIX is not a safe filename component".into(),
        });
    } else {
        config.create.directory_prefix = Some(value);
    }
}

fn map_task(
    config: &mut EffectiveConfig,
    diagnostics: &mut Vec<Diagnostic>,
    value: Value,
    name: &str,
    line: usize,
    column: usize,
) {
    let Some(value) = scalar_string(value) else {
        diagnostics.push(Diagnostic {
            line,
            column,
            message: "command must be a quoted string".into(),
        });
        return;
    };
    let argv = match tokenize_command(&value) {
        Ok(argv) if !argv.is_empty() => argv,
        Ok(_) => {
            diagnostics.push(Diagnostic {
                line,
                column,
                message: "command cannot be empty".into(),
            });
            return;
        }
        Err(message) => {
            diagnostics.push(Diagnostic {
                line,
                column,
                message,
            });
            return;
        }
    };
    let argv = match NonEmptyArgv::new(argv) {
        Ok(argv) => argv,
        Err(message) => {
            diagnostics.push(Diagnostic {
                line,
                column,
                message,
            });
            return;
        }
    };
    config.tasks.insert(
        format!("legacy_{name}"),
        Task {
            phase: TaskPhase::PostCreate,
            argv,
            cwd: None,
            required: false,
            environment_allowlist: Vec::new(),
            enabled: false,
        },
    );
}

fn tokenize_command(value: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for char in value.chars() {
        if let Some(expected) = quote {
            if char == expected {
                quote = None;
            } else if char == '\\' {
                return Err("backslash escapes are not accepted".into());
            } else {
                current.push(char);
            }
        } else if char == '\'' || char == '"' {
            quote = Some(char);
        } else if char.is_whitespace() {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else if "$`;&|<>\n()".contains(char) {
            return Err("command contains shell syntax".into());
        } else {
            current.push(char);
        }
    }
    if quote.is_some() {
        return Err("unterminated command quote".into());
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.iter().any(|token| {
        matches!(
            token.as_str(),
            "source" | "eval" | "if" | "case" | "for" | "while" | "until" | "function"
        )
    }) {
        return Err("command contains a shell control keyword".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realistic_multiline_fixture_and_maps_legacy_values() {
        let source = "SYMLINK_PATHS=(uploads .claude/settings.local.json)\nRELINK_PATHS=(CLAUDE.md .claude)\nWORKTREE_COPY_CODEGRAPH=1\nWORKTREE_SLUG_MAX=60\nWORKTREE_ROOT=\"../repo-root\"\nWORKTREE_DIR_PREFIX=\"repo-\"\nWORKTREE_INSTALL_CMD='pnpm install'\nWORKTREE_BUILD_CMD=\"pnpm exec turbo run build --filter='./packages/*'\"\n";
        let result = import_source(source, Path::new("fixture/.worktreerc")).unwrap();
        assert_eq!(
            result.config.file_rules["legacy_symlink_0"].source_root,
            SourceRoot::PrimaryWorktree
        );
        assert_eq!(
            result.config.file_rules["legacy_relink_0"].on_conflict,
            ConflictPolicy::ReplaceSymlinkOnly
        );
        assert!(result.config.file_rules.contains_key("legacy_codegraph"));
        assert_eq!(
            result.config.create.worktree_root.as_deref(),
            Some("../repo-root")
        );
        assert_eq!(
            result.config.create.directory_prefix.as_deref(),
            Some("repo-")
        );
        assert_eq!(
            result.config.tasks["legacy_build"].phase,
            TaskPhase::PostCreate
        );
        assert_eq!(
            result.config.tasks["legacy_build"].argv.as_slice(),
            [
                "pnpm",
                "exec",
                "turbo",
                "run",
                "build",
                "--filter=./packages/*"
            ]
        );
    }

    #[test]
    fn supports_comments_duplicates_and_structural_errors() {
        let result = import_source(
            "# comment\nSYMLINK_PATHS=(\n  'one' # inline\n  \"two\"\n)\nSYMLINK_PATHS=()\n",
            Path::new("fixture"),
        )
        .unwrap();
        assert_eq!(result.config.file_rules.len(), 3);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|item| item.message.contains("duplicate"))
        );
        assert!(import_source("SYMLINK_PATHS=(\"unterminated)\n", Path::new("fixture")).is_err());
        assert!(import_source("SYMLINK_PATHS=(\"x\"\n", Path::new("fixture")).is_err());
        assert!(import_source("SYMLINK_PATHS=[\"x\"]\n", Path::new("fixture")).is_err());
    }

    #[test]
    fn rejects_shell_attack_table_without_execution() {
        for attack in [
            "$(touch marker)",
            "${HOME}",
            "`touch marker`",
            "foo; touch marker",
            "foo && touch marker",
            "foo || touch marker",
            "foo | touch marker",
            "foo > marker",
            "source x",
            "eval x",
            "function x() { :; }",
            "if true; then :; fi",
            "case x in *) :;; esac",
            "for x in y; do :; done",
            "while true; do :; done",
            "until false; do :; done",
        ] {
            let result = import_source(
                &format!("WORKTREE_INSTALL_CMD=\"{attack}\"\n"),
                Path::new("fixture"),
            );
            assert!(result.is_ok() || result.is_err());
            if let Ok(result) = result {
                assert!(!result.config.tasks.contains_key("legacy_install"));
                assert!(!result.diagnostics.is_empty());
            }
            assert!(
                !Path::new("marker").exists(),
                "attack was executed: {attack}"
            );
        }
    }

    #[test]
    fn unsafe_array_discards_assignment_and_recovers_to_next_assignment() {
        let source = "SYMLINK_PATHS=(\n safe\n $(unsafe)\n)\nWORKTREE_SLUG_MAX=60\n";
        let result = import_source(source, Path::new("fixture/.worktreerc")).unwrap();
        assert!(
            !result
                .config
                .file_rules
                .keys()
                .any(|key| key.starts_with("legacy_symlink_"))
        );
        assert_eq!(result.config.create.slug_max_bytes, 60);
        assert!(result.diagnostics.iter().any(|item| item.line == 3));
    }

    #[test]
    fn quoted_unsafe_array_elements_recover_the_active_quote() {
        for source in [
            "SYMLINK_PATHS=(\n \"safe\"\n \"$(unsafe)\"\n)\nWORKTREE_SLUG_MAX=60\n",
            "SYMLINK_PATHS=(\n 'safe'\n '$(unsafe)'\n)\nWORKTREE_SLUG_MAX=60\n",
        ] {
            let result = import_source(source, Path::new("fixture/.worktreerc")).unwrap();
            assert_eq!(result.config.create.slug_max_bytes, 60);
            assert!(
                !result
                    .config
                    .file_rules
                    .keys()
                    .any(|key| key.starts_with("legacy_symlink_"))
            );
            assert!(!result.diagnostics.is_empty());
        }
    }

    #[test]
    fn quoted_unsafe_array_element_without_closing_quote_is_fatal() {
        assert!(
            import_source(
                "SYMLINK_PATHS=(\n \"$(unsafe)\n)\nWORKTREE_SLUG_MAX=60\n",
                Path::new("fixture/.worktreerc")
            )
            .is_err()
        );
    }

    #[test]
    fn scalar_quote_context_does_not_leak_into_array_recovery() {
        let source = "WORKTREE_INSTALL_CMD=\"$(unsafe)\"\nSYMLINK_PATHS=(\n safe\n $(unsafe)\n)\nWORKTREE_SLUG_MAX=60\n";
        let result = import_source(source, Path::new("fixture/.worktreerc")).unwrap();
        assert_eq!(result.config.create.slug_max_bytes, 60);
        assert!(!result.config.tasks.contains_key("legacy_install"));
        assert!(
            !result
                .config
                .file_rules
                .keys()
                .any(|key| key.starts_with("legacy_symlink_"))
        );
        assert_eq!(result.diagnostics.len(), 3);
    }

    #[test]
    fn strict_numeric_values_and_codegraph_zero_are_safe() {
        let zero = import_source(
            "WORKTREE_COPY_CODEGRAPH=0\nWORKTREE_SLUG_MAX=60\n",
            Path::new("fixture"),
        )
        .unwrap();
        assert!(!zero.config.file_rules.contains_key("legacy_codegraph"));
        let bad = import_source("WORKTREE_COPY_CODEGRAPH=2\n", Path::new("fixture")).unwrap();
        assert!(!bad.config.file_rules.contains_key("legacy_codegraph"));
        assert!(
            bad.diagnostics
                .iter()
                .any(|item| item.message.contains("strict"))
        );
    }

    #[test]
    fn repository_fixtures_are_parsed_without_external_paths() {
        let kesher = import_source(
            include_str!("../tests/fixtures/kesher.worktreerc.example"),
            Path::new("kesher"),
        )
        .unwrap();
        assert_eq!(kesher.config.create.slug_max_bytes, 60);
        assert_eq!(
            kesher.config.tasks["legacy_install"].phase,
            TaskPhase::PostCreate
        );
        let globo = import_source(
            include_str!("../tests/fixtures/globo-skills.worktreerc"),
            Path::new("globo"),
        )
        .unwrap();
        assert!(globo.config.file_rules.contains_key("legacy_codegraph"));
        assert_eq!(
            globo.config.file_rules["legacy_symlink_0"].source_root,
            SourceRoot::PrimaryWorktree
        );
    }
}
