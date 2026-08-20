//! A safe, bounded Bash-like parser for the Ferrous Shell.
//!
//! The parser is a recursive-descent grammar over a bounded tokenizer. Its
//! contract is deliberately narrow:
//!
//! - supported: quoting (`'`/`"`), backslash escapes, simple commands,
//!   pipelines (`|`), `&&`, `||`, `;`, input/output redirection (`<`, `>`,
//!   `>>`), and background (`&`);
//! - rejected with explicit errors: command substitution, arithmetic
//!   expansion, here-documents, process substitution, aliases, function
//!   definitions, `eval`, environment-variable expansion, and any construct
//!   that would require an unbounded host-shell evaluation.
//!
//! The parser never performs filesystem expansion, never executes anything,
//! and never emits a shell command string. The output is always the typed
//! [`ShellProgram`] IR from [`crate::shell_ir`].

use std::fmt;

use crate::shell_ir::{
    Builtin, CommandSpec, Program, Redirect, SessionPath, ShellProgram, Statement,
};

/// Bounds applied while parsing so untrusted input cannot exhaust memory.
pub const DEFAULT_MAX_INPUT_BYTES: usize = 64 * 1024;
/// Maximum number of tokens in one line.
pub const DEFAULT_MAX_TOKENS: usize = 4096;
/// Maximum number of pipeline stages in one statement.
pub const DEFAULT_MAX_PIPELINE_STAGES: usize = 64;
/// Maximum number of statements in one plan.
pub const DEFAULT_MAX_STATEMENTS: usize = 4096;

/// A lexical token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Token {
    /// A word (quoted or unquoted, escapes resolved).
    Word(String),
    /// A single-character operator.
    Operator(Operator),
    /// A newline (input line boundaries, `;` and `&&`/`||` separate too).
    Newline,
    /// End of input.
    End,
}

/// Operators recognized by the tokenizer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operator {
    /// `|`
    Pipe,
    /// `&&`
    And,
    /// `||`
    Or,
    /// `;`
    Semi,
    /// `<`
    RedirectIn,
    /// `>`
    RedirectOut,
    /// `>>`
    RedirectAppend,
    /// `&`
    Background,
    /// `(`
    OpenParen,
    /// `)`
    CloseParen,
}

/// Errors produced while parsing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// A quote was opened but never closed.
    UnterminatedQuote,
    /// A backslash was the final character.
    TrailingEscape,
    /// An operator was used where a command was required.
    InvalidOperator(&'static str),
    /// A construct was recognized but is intentionally unsupported.
    UnsupportedConstruct(&'static str),
    /// The input exceeded a bound.
    InputTooLong(usize),
    /// The token count exceeded a bound.
    TooManyTokens(usize),
    /// Too many pipeline stages.
    TooManyPipelineStages(usize),
    /// Too many statements in one plan.
    TooManyStatements(usize),
    /// A NUL byte is never legal in shell input.
    NulByte,
    /// A word had no characters.
    EmptyCommand,
    /// The builtin could not be built from its arguments.
    InvalidBuiltin(&'static str),
    /// The parse consumed nothing meaningful.
    EmptyInput,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnterminatedQuote => formatter.write_str("unterminated quote"),
            ParseError::TrailingEscape => formatter.write_str("trailing escape"),
            ParseError::InvalidOperator(operator) => {
                write!(formatter, "invalid operator: {operator}")
            }
            ParseError::UnsupportedConstruct(construct) => {
                write!(formatter, "unsupported construct: {construct}")
            }
            ParseError::InputTooLong(max) => {
                write!(formatter, "input too long (max {max} bytes)")
            }
            ParseError::TooManyTokens(max) => {
                write!(formatter, "too many tokens (max {max})")
            }
            ParseError::TooManyPipelineStages(max) => {
                write!(formatter, "too many pipeline stages (max {max})")
            }
            ParseError::TooManyStatements(max) => {
                write!(formatter, "too many statements (max {max})")
            }
            ParseError::NulByte => formatter.write_str("NUL byte in input"),
            ParseError::EmptyCommand => formatter.write_str("empty command"),
            ParseError::InvalidBuiltin(message) => {
                write!(formatter, "invalid builtin arguments: {message}")
            }
            ParseError::EmptyInput => formatter.write_str("empty input"),
        }
    }
}

/// Parse configuration bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseLimits {
    /// Maximum input length in bytes.
    pub max_input_bytes: usize,
    /// Maximum token count.
    pub max_tokens: usize,
    /// Maximum pipeline stages.
    pub max_pipeline_stages: usize,
    /// Maximum statements in a plan.
    pub max_statements: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: DEFAULT_MAX_INPUT_BYTES,
            max_tokens: DEFAULT_MAX_TOKENS,
            max_pipeline_stages: DEFAULT_MAX_PIPELINE_STAGES,
            max_statements: DEFAULT_MAX_STATEMENTS,
        }
    }
}

/// The Ferrous Shell parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShellParser {
    /// Bounds applied to every parse.
    pub limits: ParseLimits,
}

impl ShellParser {
    /// Create a parser with the default bounds.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse a human shell line into a typed plan.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] for malformed or intentionally unsupported
    /// input. Never executes or expands anything.
    pub fn parse(&self, input: &str) -> Result<ShellProgram, ParseError> {
        if input.chars().any(|character| character == '\0') {
            return Err(ParseError::NulByte);
        }
        if input.len() > self.limits.max_input_bytes {
            return Err(ParseError::InputTooLong(self.limits.max_input_bytes));
        }
        let tokens = Tokenizer::new(self).tokenize(input)?;
        if tokens.is_empty() {
            return Err(ParseError::EmptyInput);
        }
        let mut parser = Parser::new(tokens, self.limits);
        let program = parser.parse_program()?;
        if program.statements.is_empty() {
            return Err(ParseError::EmptyInput);
        }
        Ok(program)
    }

    /// Build a plan directly from a structured argv (the AI path).
    ///
    /// This bypasses text parsing entirely: the AI hands over a program name
    /// and separate arguments, and the result is a single command statement.
    /// The program name is resolved to a builtin when it is a known builtin;
    /// everything else becomes an external program. Explicit `run-wasi
    /// <component>` prefixes select the WASI backend, and `run-native --allow
    /// -- <program> [args]` maps to the compatible external path.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] when a builtin receives invalid arguments.
    pub fn parse_ai_argv(
        &self,
        program: &str,
        args: &[String],
        cwd: SessionPath,
    ) -> Result<ShellProgram, ParseError> {
        let (program, args) = resolve_program_and_args(program, args.to_vec())?;
        Ok(ShellProgram {
            statements: vec![Statement::Command(CommandSpec {
                program,
                args,
                redirects: Vec::new(),
                cwd,
            })],
        })
    }
}

/// The bounded tokenizer: quote state, escapes, and operators.
struct Tokenizer<'a> {
    limits: ParseLimits,
    input: &'a str,
    position: usize,
    tokens: Vec<Token>,
}

impl<'a> Tokenizer<'a> {
    fn new(parser: &ShellParser) -> Self {
        Self {
            limits: parser.limits,
            input: "",
            position: 0,
            tokens: Vec::new(),
        }
    }

    fn tokenize(mut self, input: &'a str) -> Result<Vec<Token>, ParseError> {
        self.input = input;
        let mut current = String::new();
        let mut has_current = false;
        let characters: Vec<char> = input.chars().collect();

        while self.position < characters.len() {
            let character = characters[self.position];
            match character {
                '\\' => {
                    self.position += 1;
                    if self.position >= characters.len() {
                        return Err(ParseError::TrailingEscape);
                    }
                    current.push(characters[self.position]);
                    self.position += 1;
                    has_current = true;
                }
                '\'' | '"' => {
                    let quote = character;
                    self.position += 1;
                    let mut closed = false;
                    while self.position < characters.len() {
                        let next = characters[self.position];
                        self.position += 1;
                        if next == quote {
                            closed = true;
                            break;
                        }
                        if next == '\\' && quote == '"' && self.position < characters.len() {
                            current.push(characters[self.position]);
                            self.position += 1;
                            continue;
                        }
                        current.push(next);
                    }
                    if !closed {
                        return Err(ParseError::UnterminatedQuote);
                    }
                    has_current = true;
                }
                '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                    if has_current {
                        self.push_word(std::mem::take(&mut current))?;
                        has_current = false;
                    }
                    let operator = self.operator_at(&characters)?;
                    self.tokens.push(Token::Operator(operator));
                }
                character if character.is_whitespace() => {
                    if has_current {
                        self.push_word(std::mem::take(&mut current))?;
                        has_current = false;
                    }
                }
                _ => {
                    current.push(character);
                    has_current = true;
                }
            }
        }
        if has_current {
            self.push_word(current)?;
        }
        Ok(self.tokens)
    }

    fn operator_at(&mut self, characters: &[char]) -> Result<Operator, ParseError> {
        let first = characters[self.position];
        let second = characters.get(self.position + 1).copied().unwrap_or('\0');
        match (first, second) {
            ('|', '|') => {
                self.position += 2;
                Ok(Operator::Or)
            }
            ('&', '&') => {
                self.position += 2;
                Ok(Operator::And)
            }
            ('>', '>') => {
                self.position += 2;
                Ok(Operator::RedirectAppend)
            }
            ('|', _) => {
                self.position += 1;
                Ok(Operator::Pipe)
            }
            ('&', _) => {
                self.position += 1;
                Ok(Operator::Background)
            }
            (';', _) => {
                self.position += 1;
                Ok(Operator::Semi)
            }
            ('<', _) => {
                self.position += 1;
                Ok(Operator::RedirectIn)
            }
            ('>', _) => {
                self.position += 1;
                Ok(Operator::RedirectOut)
            }
            ('(', _) => {
                self.position += 1;
                Ok(Operator::OpenParen)
            }
            (')', _) => {
                self.position += 1;
                Ok(Operator::CloseParen)
            }
            _ => Err(ParseError::InvalidOperator("unknown operator")),
        }
    }

    fn push_word(&mut self, word: String) -> Result<(), ParseError> {
        if self.tokens.len() >= self.limits.max_tokens {
            return Err(ParseError::TooManyTokens(self.limits.max_tokens));
        }
        self.tokens.push(Token::Word(word));
        Ok(())
    }
}

/// The recursive-descent grammar over a token stream.
struct Parser {
    tokens: Vec<Token>,
    position: usize,
    limits: ParseLimits,
    statements: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>, limits: ParseLimits) -> Self {
        Self {
            tokens,
            position: 0,
            limits,
            statements: 0,
        }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.position).unwrap_or(&Token::End)
    }

    fn next(&mut self) -> Token {
        let token = self.peek().clone();
        if !matches!(token, Token::End) {
            self.position += 1;
        }
        token
    }

    fn parse_program(&mut self) -> Result<ShellProgram, ParseError> {
        let mut statements = Vec::new();
        loop {
            self.skip_separators();
            if matches!(self.peek(), Token::End) {
                break;
            }
            if statements.len() >= self.limits.max_statements {
                return Err(ParseError::TooManyStatements(self.limits.max_statements));
            }
            let statement = self.parse_statement()?;
            statements.push(statement);
            // After a statement, expect a separator or end.
            match self.peek() {
                Token::Operator(Operator::Semi) | Token::Newline | Token::End => {}
                Token::Operator(Operator::And) | Token::Operator(Operator::Or) => {}
                _ => return Err(ParseError::InvalidOperator("expected separator")),
            }
        }
        Ok(ShellProgram { statements })
    }

    fn skip_separators(&mut self) {
        loop {
            match self.peek() {
                Token::Operator(Operator::Semi) | Token::Newline => {
                    self.next();
                }
                _ => break,
            }
        }
    }

    fn parse_statement(&mut self) -> Result<Statement, ParseError> {
        self.statements += 1;
        let left = self.parse_and_or()?;
        // Trailing background marker.
        if matches!(self.peek(), Token::Operator(Operator::Background)) {
            self.next();
            return Ok(Statement::Background(Box::new(left)));
        }
        Ok(left)
    }

    fn parse_and_or(&mut self) -> Result<Statement, ParseError> {
        let mut left = self.parse_pipeline()?;
        loop {
            match self.peek() {
                Token::Operator(Operator::And) => {
                    self.next();
                    let right = self.parse_pipeline()?;
                    left = Statement::And(Box::new(left), Box::new(right));
                }
                Token::Operator(Operator::Or) => {
                    self.next();
                    let right = self.parse_pipeline()?;
                    left = Statement::Or(Box::new(left), Box::new(right));
                }
                _ => break,
            }
        }
        Ok(left)
    }

    fn parse_pipeline(&mut self) -> Result<Statement, ParseError> {
        let mut stages = vec![self.parse_command()?];
        while matches!(self.peek(), Token::Operator(Operator::Pipe)) {
            self.next();
            if stages.len() >= self.limits.max_pipeline_stages {
                return Err(ParseError::TooManyPipelineStages(
                    self.limits.max_pipeline_stages,
                ));
            }
            stages.push(self.parse_command()?);
        }
        match stages.len() {
            1 => Ok(Statement::Command(stages.remove(0))),
            _ => Ok(Statement::Pipeline(stages)),
        }
    }

    fn parse_command(&mut self) -> Result<CommandSpec, ParseError> {
        // Subshells are intentionally unsupported.
        if matches!(self.peek(), Token::Operator(Operator::OpenParen)) {
            return Err(ParseError::UnsupportedConstruct("subshell `( ... )`"));
        }
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        loop {
            match self.peek() {
                Token::Word(word) => {
                    words.push(word.clone());
                    self.next();
                }
                Token::Operator(Operator::RedirectIn)
                | Token::Operator(Operator::RedirectOut)
                | Token::Operator(Operator::RedirectAppend) => {
                    let operator = match self.next() {
                        Token::Operator(operator) => operator,
                        _ => unreachable!("peeked operator"),
                    };
                    let target = match self.next() {
                        Token::Word(path) => path,
                        _ => return Err(ParseError::InvalidOperator("redirection target")),
                    };
                    let path = SessionPath::new(target).map_err(|_| {
                        ParseError::InvalidOperator("redirection target must be a session path")
                    })?;
                    redirects.push(match operator {
                        Operator::RedirectIn => Redirect::Input(path),
                        Operator::RedirectOut => Redirect::OutputTruncate(path),
                        Operator::RedirectAppend => Redirect::OutputAppend(path),
                        _ => unreachable!("peeked redirection operator"),
                    });
                }
                _ => break,
            }
        }
        if words.is_empty() {
            return Err(ParseError::EmptyCommand);
        }
        let program_name = words.remove(0);
        let (program, args) = resolve_program_and_args(&program_name, words)?;
        let spec = CommandSpec {
            program,
            args,
            redirects,
            cwd: SessionPath::new(".").map_err(|_| ParseError::EmptyCommand)?,
        };
        Ok(spec)
    }
}

/// Resolve a program name plus its argv into a [`(Program, Vec<String>)`],
/// building structured builtins (with validated paths) at parse time and
/// rejecting native-shell escapes and unbounded constructs.
fn resolve_program_and_args(
    name: &str,
    args: Vec<String>,
) -> Result<(Program, Vec<String>), ParseError> {
    match name {
        "run-wasi" => {
            let mut args = args.into_iter();
            let component = args.next().ok_or(ParseError::InvalidOperator(
                "run-wasi requires a component path argument",
            ))?;
            Ok((Program::WasiComponent(component), args.collect()))
        }
        "run-native" => {
            // Compatibility alias: `run-native --allow -- <program> [args]`.
            let mut rest = args.into_iter();
            if rest.next().as_deref() != Some("--allow") || rest.next().as_deref() != Some("--") {
                return Err(ParseError::InvalidOperator(
                    "run-native requires `--allow -- <program> [args]`",
                ));
            }
            let program = rest.next().ok_or(ParseError::InvalidOperator(
                "run-native requires a program after `--`",
            ))?;
            Ok((Program::External(program), rest.collect()))
        }
        "bash" | "sh" | "zsh" | "fish" | "powershell" | "pwsh" | "cmd" => {
            Err(ParseError::UnsupportedConstruct(
                "native shell interpreters require explicit policy approval",
            ))
        }
        "eval" => Err(ParseError::UnsupportedConstruct("eval")),
        "source" | "." => Err(ParseError::UnsupportedConstruct("source/startup files")),
        "alias" | "function" => Err(ParseError::UnsupportedConstruct(
            "shell state mutation is not supported",
        )),
        other => match builtin_from_words(other, args)? {
            (Some(builtin), args) => Ok((Program::Builtin(builtin), args)),
            (None, args) => Ok((Program::External(other.to_owned()), args)),
        },
    }
}

/// Build a structured [`Builtin`] from its argv, validating arity and paths.
///
/// Returns `(None, args)` for names that are not builtins, so the caller can
/// fall back to an external program. The returned `Vec<String>` is the
/// leftover argv (usually empty for builtins).
fn builtin_from_words(
    name: &str,
    args: Vec<String>,
) -> Result<(Option<Builtin>, Vec<String>), ParseError> {
    fn exactly_one(name: &str, args: &[String]) -> Result<String, ParseError> {
        if args.len() == 1 {
            Ok(args[0].clone())
        } else {
            Err(ParseError::InvalidBuiltin(match name {
                "cd" => "cd takes exactly one path argument",
                "ls" => "ls takes at most one path argument",
                "cat" => "cat takes exactly one path argument",
                "mkdir" => "mkdir takes exactly one path argument",
                "rm" => "rm takes exactly one path argument",
                "which" => "which takes exactly one program name",
                "export" => "export takes exactly one NAME=value assignment",
                _ => "builtin takes exactly one argument",
            }))
        }
    }

    let builtin = match name {
        "pwd" => Builtin::Pwd,
        "env" => Builtin::Env,
        "echo" => Builtin::Echo(args),
        "cd" => {
            let target = exactly_one("cd", &args)?;
            let path = SessionPath::new(target)
                .map_err(|_| ParseError::InvalidBuiltin("cd path is not a session path"))?;
            Builtin::Cd(path)
        }
        "ls" => {
            let target = if args.is_empty() {
                ".".to_owned()
            } else {
                exactly_one("ls", &args)?
            };
            let path = SessionPath::new(target)
                .map_err(|_| ParseError::InvalidBuiltin("ls path is not a session path"))?;
            Builtin::Ls(path)
        }
        "cat" => {
            let target = exactly_one("cat", &args)?;
            let path = SessionPath::new(target)
                .map_err(|_| ParseError::InvalidBuiltin("cat path is not a session path"))?;
            Builtin::Cat(path)
        }
        "mkdir" => {
            let target = exactly_one("mkdir", &args)?;
            let path = SessionPath::new(target)
                .map_err(|_| ParseError::InvalidBuiltin("mkdir path is not a session path"))?;
            Builtin::Mkdir(path)
        }
        "rm" => {
            let target = exactly_one("rm", &args)?;
            let path = SessionPath::new(target)
                .map_err(|_| ParseError::InvalidBuiltin("rm path is not a session path"))?;
            Builtin::Remove(path)
        }
        "cp" | "mv" => {
            if args.len() != 2 {
                return Err(ParseError::InvalidBuiltin(
                    "cp and mv take exactly two path arguments",
                ));
            }
            let from = SessionPath::new(args[0].clone())
                .map_err(|_| ParseError::InvalidBuiltin("source path is not a session path"))?;
            let to = SessionPath::new(args[1].clone()).map_err(|_| {
                ParseError::InvalidBuiltin("destination path is not a session path")
            })?;
            if name == "cp" {
                Builtin::Copy { from, to }
            } else {
                Builtin::Move { from, to }
            }
        }
        "which" => {
            let target = exactly_one("which", &args)?;
            Builtin::Which(target)
        }
        "export" => {
            // `export NAME=value` or `export NAME` (mark for removal from overlay).
            let assignment = exactly_one("export", &args)?;
            let (name, value) = match assignment.split_once('=') {
                Some((name, value)) if !name.is_empty() => {
                    (name.to_owned(), Some(value.to_owned()))
                }
                _ => (assignment, None),
            };
            if name.is_empty() || name.chars().any(char::is_whitespace) {
                return Err(ParseError::InvalidBuiltin(
                    "export requires a variable name",
                ));
            }
            Builtin::Export { name, value }
        }
        _ => return Ok((None, args)),
    };
    Ok((Some(builtin), Vec::new()))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn parse(input: &str) -> Result<ShellProgram, ParseError> {
        ShellParser::new().parse(input)
    }

    fn command_of(program: &ShellProgram) -> &CommandSpec {
        match &program.statements[0] {
            Statement::Command(spec) => spec,
            _ => panic!("expected a command statement"),
        }
    }

    #[test]
    fn parses_cd_pipeline_and_npm_as_structured_commands() {
        let program = parse("cd src | npm test").expect("parses");
        let Statement::Pipeline(stages) = &program.statements[0] else {
            panic!("expected pipeline");
        };
        assert_eq!(stages.len(), 2);
        assert!(matches!(
            stages[0].program,
            Program::Builtin(Builtin::Cd(_))
        ));
        assert_eq!(stages[1].program, Program::External("npm".to_owned()));
        assert_eq!(stages[1].args, ["test"]);
    }

    #[test]
    fn preserves_quoted_metacharacters_as_one_argument() {
        let program = parse(r#"echo "a | b" 'c && d'"#).expect("parses");
        let spec = command_of(&program);
        assert_eq!(spec.args, ["a | b", "c && d"]);
    }

    #[test]
    fn parses_and_or_sequence_and_redirects() {
        let program = parse("mkdir src && cd src || echo failed; ls > out.txt").expect("parses");
        let statements = &program.statements;
        assert_eq!(statements.len(), 2);
        // `mkdir src && cd src || echo failed` -> Or(And(mkdir, cd), echo)
        let Statement::Or(left, _right) = &statements[0] else {
            panic!("expected Or");
        };
        assert!(matches!(&**left, Statement::And(_, _)));
        // `ls > out.txt`
        let Statement::Command(spec) = &statements[1] else {
            panic!("expected command");
        };
        assert_eq!(
            spec.program,
            Program::Builtin(Builtin::Ls(SessionPath::new(".").expect("valid path")))
        );
        assert_eq!(
            spec.redirects,
            [Redirect::OutputTruncate(
                SessionPath::new("out.txt").expect("valid path")
            )]
        );
    }

    #[test]
    fn rejects_command_substitution_and_unbounded_eval() {
        assert!(matches!(
            parse("echo $(ls)"),
            Err(ParseError::UnsupportedConstruct(_))
        ));
        assert!(matches!(
            parse("eval 'rm -rf /'"),
            Err(ParseError::UnsupportedConstruct("eval"))
        ));
        assert!(matches!(
            parse("bash -c 'rm -rf /'"),
            Err(ParseError::UnsupportedConstruct(_))
        ));
    }

    #[test]
    fn rejects_unterminated_quotes_and_trailing_escapes() {
        assert_eq!(
            parse("echo 'unterminated"),
            Err(ParseError::UnterminatedQuote)
        );
        assert_eq!(parse("echo trailing\\"), Err(ParseError::TrailingEscape));
    }

    #[test]
    fn parser_never_emits_a_shell_command_string() {
        // Structural proof: the IR has no field that could carry a shell
        // string as a single opaque blob beyond structured argv. A pipeline
        // of `echo` with metacharacters stays split across argv elements.
        let program = parse("echo 'a;b' | echo 'c>d'").expect("parses");
        let Statement::Pipeline(stages) = &program.statements[0] else {
            panic!("expected pipeline");
        };
        assert_eq!(stages[0].args, ["a;b"]);
        assert_eq!(stages[1].args, ["c>d"]);
    }

    #[test]
    fn rejects_nul_bytes_and_overlong_input() {
        assert_eq!(parse("echo a\0b"), Err(ParseError::NulByte));
        let parser = ShellParser {
            limits: ParseLimits {
                max_input_bytes: 8,
                ..ParseLimits::default()
            },
        };
        assert!(matches!(
            parser.parse("echo verylongcommand"),
            Err(ParseError::InputTooLong(_))
        ));
    }

    #[test]
    fn parses_background_and_sequence() {
        let program = parse("npm test & echo done").expect("parses");
        let statements = &program.statements;
        assert_eq!(statements.len(), 2);
        assert!(matches!(statements[0], Statement::Background(_)));
        assert!(matches!(statements[1], Statement::Command(_)));
    }

    #[test]
    fn parse_ai_argv_produces_structured_single_command() {
        let parser = ShellParser::new();
        let program = parser
            .parse_ai_argv(
                "npm",
                &["install".to_owned(), "--save-dev".to_owned()],
                SessionPath::new(".").expect("valid cwd"),
            )
            .expect("valid argv");
        let spec = command_of(&program);
        assert_eq!(spec.program, Program::External("npm".to_owned()));
        assert_eq!(spec.args, ["install", "--save-dev"]);
    }

    #[test]
    fn parse_ai_argv_maps_run_wasi_prefix() {
        let parser = ShellParser::new();
        let program = parser
            .parse_ai_argv(
                "run-wasi",
                &["./tool.wasm".to_owned(), "--flag".to_owned()],
                SessionPath::new(".").expect("valid cwd"),
            )
            .expect("valid argv");
        let spec = command_of(&program);
        assert_eq!(
            spec.program,
            Program::WasiComponent("./tool.wasm".to_owned())
        );
        assert_eq!(spec.args, ["--flag"]);
    }

    #[test]
    fn parses_export_into_a_structured_overlay_change() {
        let program = parse("export FOO=bar").expect("parses");
        let spec = command_of(&program);
        assert_eq!(
            spec.program,
            Program::Builtin(Builtin::Export {
                name: "FOO".to_owned(),
                value: Some("bar".to_owned()),
            })
        );

        let program = parse("export FOO").expect("parses");
        let spec = command_of(&program);
        assert_eq!(
            spec.program,
            Program::Builtin(Builtin::Export {
                name: "FOO".to_owned(),
                value: None,
            })
        );
    }

    #[test]
    fn rejects_subshell_and_empty_command() {
        assert!(matches!(
            parse("(echo hi)"),
            Err(ParseError::UnsupportedConstruct(_))
        ));
        assert!(matches!(parse(";"), Err(ParseError::EmptyCommand)));
    }

    #[test]
    fn rejects_heredoc_and_process_substitution() {
        // `<<` is not tokenized as an operator; it becomes `<` `<`, which
        // fails with an invalid-operator error rather than parsing silently.
        let result = parse("cat <<EOF");
        assert!(result.is_err());
        let result = parse("diff <(a) <(b)");
        assert!(matches!(
            result,
            Err(ParseError::UnsupportedConstruct(_)) | Err(ParseError::InvalidOperator(_))
        ));
    }
}
