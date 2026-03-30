//! Command output filter engine.
//!
//! Reduces noisy command output to the most relevant lines, saving tokens in the
//! context window. Each tool invocation's full output is stored separately by the
//! caller; this module only produces the compact version that goes into the
//! conversation.

use regex::Regex;
use std::sync::OnceLock;

/// Filter command output to reduce tokens. Returns the filtered output.
/// The caller stores the full original if needed.
pub fn filter(command: &str, output: &str) -> String {
    // Extract the effective command from shell wrappers like
    // "cd /tmp/foo && CARGO_BUILD_JOBS=1 cargo check ..."
    let effective = extract_effective_command(command);

    // Check cargo first (structural parser)
    if let Some(filtered) = try_filter_cargo(&effective, output) {
        return filtered;
    }

    // Try static filters
    for (idx, f) in FILTERS.iter().enumerate() {
        if let Ok(re) = Regex::new(f.command_pattern) {
            if re.is_match(&effective) {
                return apply_filter(f, idx, output);
            }
        }
    }

    // Fallback: generic line dedup + truncation
    generic_filter(output)
}

/// Extract the effective command from shell wrappers.
///
/// Handles patterns like:
/// - `cd /tmp/foo && cargo check`
/// - `cd /tmp/foo && ENV_VAR=val cargo build`
/// - `ENV_VAR=val cargo test`
/// - `timeout 120 cargo clippy`
fn extract_effective_command(command: &str) -> String {
    // Take the last command in a `&&` chain.
    let last = command.rsplit("&&").next().unwrap_or(command).trim();

    // Strip leading env var assignments (KEY=val) and `timeout N`.
    let words: Vec<&str> = last.split_whitespace().collect();
    let mut skip = 0;
    while skip < words.len() {
        let word = words[skip];
        if word.contains('=') {
            skip += 1;
        } else if word == "timeout" {
            skip += 1;
            // `timeout` also consumes its numeric argument
            if skip < words.len() && words[skip].parse::<u64>().is_ok() {
                skip += 1;
            }
        } else {
            break;
        }
    }
    words[skip..].join(" ")
}

// ---------------------------------------------------------------------------
// Static filter definitions (converted from RTK TOML files)
// ---------------------------------------------------------------------------

struct CommandFilter {
    command_pattern: &'static str,
    strip_patterns: &'static [&'static str],
    strip_ansi: bool,
    max_line_len: usize, // 0 = no limit
    max_lines: usize,    // 0 = no limit
    on_empty: &'static str,
}

static FILTERS: &[CommandFilter] = &[
    // 1. ansible-playbook
    CommandFilter {
        command_pattern: r"^ansible-playbook\b",
        strip_patterns: &[r"^\s*$", r"^ok: \[", r"^skipping: \["],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 60,
        on_empty: "",
    },
    // 2. basedpyright
    CommandFilter {
        command_pattern: r"^basedpyright\b",
        strip_patterns: &[
            r"^\s*$",
            r"^Searching for source files",
            r"^Found \d+ source file",
            r"^Pyright \d+\.\d+",
            r"^basedpyright \d+\.\d+",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "basedpyright: ok",
    },
    // 3. biome
    CommandFilter {
        command_pattern: r"^biome\b",
        strip_patterns: &[
            r"^\s*$",
            r"^Checked \d+ file",
            r"^Fixed \d+ file",
            r"^The following command",
            r"^Run it with",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "biome: ok",
    },
    // 4. brew install/upgrade
    CommandFilter {
        command_pattern: r"^brew\s+(install|upgrade)\b",
        strip_patterns: &[
            r"^\s*$",
            r"^==> Downloading",
            r"^==> Pouring",
            r"^Already downloaded:",
            r"^###",
            r"^==> Fetching",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "",
    },
    // 5. bundle install/update
    CommandFilter {
        command_pattern: r"^bundle\s+(install|update)\b",
        strip_patterns: &[
            r"^Using ",
            r"^\s*$",
            r"^Fetching gem metadata",
            r"^Resolving dependencies",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "",
    },
    // 6. composer install/update/require
    CommandFilter {
        command_pattern: r"^composer\s+(install|update|require)\b",
        strip_patterns: &[
            r"^\s*$",
            r"^  - Downloading ",
            r"^  - Installing ",
            r"^Loading composer",
            r"^Updating dependencies",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "",
    },
    // 7. df
    CommandFilter {
        command_pattern: r"^df(\s|$)",
        strip_patterns: &[],
        strip_ansi: true,
        max_line_len: 80,
        max_lines: 20,
        on_empty: "",
    },
    // 8. dotnet build
    CommandFilter {
        command_pattern: r"^dotnet\s+build\b",
        strip_patterns: &[
            r"^\s*$",
            r"^Microsoft \(R\)",
            r"^Copyright \(C\)",
            r"^  Determining projects",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 40,
        on_empty: "",
    },
    // 9. du
    CommandFilter {
        command_pattern: r"^du\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: false,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 10. fail2ban-client
    CommandFilter {
        command_pattern: r"^fail2ban-client\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: false,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "",
    },
    // 11. gcc/g++
    CommandFilter {
        command_pattern: r"^g(cc|\+\+)\b",
        strip_patterns: &[
            r"^\s*$",
            r"^\s+\|\s*$",
            r"^In file included from",
            r"^\s+from\s",
            r"^\d+ warnings? generated",
            r"^\d+ errors? generated",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "gcc: ok",
    },
    // 12. gcloud
    CommandFilter {
        command_pattern: r"^gcloud\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 30,
        on_empty: "",
    },
    // 13. gradle/gradlew
    CommandFilter {
        command_pattern: r"^(gradle|gradlew|\./gradlew?)\b",
        strip_patterns: &[
            r"^\s*$",
            r"^> Configuring project",
            r"^> Resolving dependencies",
            r"^> Transform ",
            r"^Download(ing)?\s+http",
            r"^\s*<-+>\s*$",
            r"^> Task :.*UP-TO-DATE$",
            r"^> Task :.*NO-SOURCE$",
            r"^> Task :.*FROM-CACHE$",
            r"^Starting a Gradle Daemon",
            r"^Daemon will be stopped",
        ],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 50,
        on_empty: "gradle: ok",
    },
    // 14. hadolint
    CommandFilter {
        command_pattern: r"^hadolint\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 15. helm
    CommandFilter {
        command_pattern: r"^helm\b",
        strip_patterns: &[r"^\s*$", r"^W\d{4}"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 16. iptables
    CommandFilter {
        command_pattern: r"^iptables\b",
        strip_patterns: &[r"^\s*$", r"^Chain DOCKER", r"^Chain BR-"],
        strip_ansi: false,
        max_line_len: 120,
        max_lines: 50,
        on_empty: "",
    },
    // 17. jira
    CommandFilter {
        command_pattern: r"^jira\b",
        strip_patterns: &[r"^\s*$", r"^\s*--"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 18. jj (Jujutsu)
    CommandFilter {
        command_pattern: r"^jj\b",
        strip_patterns: &[r"^\s*$", r"^Hint:", r"^Working copy now at:"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 30,
        on_empty: "",
    },
    // 19. jq
    CommandFilter {
        command_pattern: r"^jq\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 20. just
    CommandFilter {
        command_pattern: r"^just\b",
        strip_patterns: &[r"^\s*$", r"^\s*Available recipes:", r"^\s*just --list"],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 50,
        on_empty: "",
    },
    // 21. make
    CommandFilter {
        command_pattern: r"^make\b",
        strip_patterns: &[r"^make\[\d+\]:", r"^\s*$", r"^Nothing to be done"],
        strip_ansi: false,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "make: ok",
    },
    // 22. markdownlint
    CommandFilter {
        command_pattern: r"^markdownlint\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 50,
        on_empty: "",
    },
    // 23. mise
    CommandFilter {
        command_pattern: r"^mise\s+(run|exec|install|upgrade)\b",
        strip_patterns: &[
            r"^\s*$",
            "^mise\\s+(trust|install|upgrade).*\u{2713}",
            r"^mise\s+Installing\s",
            r"^mise\s+Downloading\s",
            r"^mise\s+Extracting\s",
            r"^mise\s+\w+@[\d.]+ installed",
        ],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 50,
        on_empty: "mise: ok",
    },
    // 24. mix compile
    CommandFilter {
        command_pattern: r"^mix\s+compile(\s|$)",
        strip_patterns: &[r"^Compiling \d+ file", r"^\s*$", r"^Generated\s"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 40,
        on_empty: "mix compile: ok",
    },
    // 25. mix format
    CommandFilter {
        command_pattern: r"^mix\s+format(\s|$)",
        strip_patterns: &[],
        strip_ansi: false,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "mix format: ok",
    },
    // 26. mvn compile/package/clean/install
    CommandFilter {
        command_pattern: r"^mvn\s+(compile|package|clean|install)\b",
        strip_patterns: &[
            r"^\[INFO\] ---",
            r"^\[INFO\] Building\s",
            r"^\[INFO\] Downloading\s",
            r"^\[INFO\] Downloaded\s",
            r"^\[INFO\]\s*$",
            r"^\s*$",
            r"^Downloading:",
            r"^Downloaded:",
            r"^Progress",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "mvn: ok",
    },
    // 27. nx
    CommandFilter {
        command_pattern: r"^(pnpm\s+)?nx\b",
        strip_patterns: &[
            r"^\s*$",
            r"^\s*>\s*NX\s+Running target",
            r"^\s*>\s*NX\s+Nx read the output",
            r"^\s*>\s*NX\s+View logs",
            "\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}\u{2014}",
            r"^\s+Nx \(powered by",
        ],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 60,
        on_empty: "",
    },
    // 28. ollama run
    CommandFilter {
        command_pattern: r"^ollama\s+run\b",
        strip_patterns: &[
            "^[\u{280B}\u{2819}\u{2839}\u{2838}\u{283C}\u{2834}\u{2826}\u{2827}\u{2807}\u{280F}\\s]*$",
            r"^\s*$",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 0,
        on_empty: "",
    },
    // 29. oxlint
    CommandFilter {
        command_pattern: r"^oxlint\b",
        strip_patterns: &[r"^\s*$", r"^Finished in \d+", r"^Found \d+ warning"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "oxlint: ok",
    },
    // 30. ping
    CommandFilter {
        command_pattern: r"^ping\b",
        strip_patterns: &[
            r"^PING ",
            r"^Pinging ",
            r"^\d+ bytes from ",
            r"^Reply from .+: bytes=",
            r"^\s*$",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 0,
        on_empty: "",
    },
    // 31. pio run
    CommandFilter {
        command_pattern: r"^pio\s+run",
        strip_patterns: &[
            r"^\s*$",
            r"^Verbose mode",
            r"^CONFIGURATION:",
            r"^LDF:",
            r"^Library Manager:",
            r"^Compiling\s",
            r"^Linking\s",
            r"^Building\s",
            r"^Checking size",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "pio run: ok",
    },
    // 32. poetry install/lock/update
    CommandFilter {
        command_pattern: r"^poetry\s+(install|lock|update)\b",
        strip_patterns: &[
            r"^\s*$",
            "^  [-\u{2022}] Downloading ",
            "^  [-\u{2022}] Installing .* \\(",
            r"^Creating virtualenv",
            r"^Using virtualenv",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "",
    },
    // 33. pre-commit
    CommandFilter {
        command_pattern: r"^pre-commit\b",
        strip_patterns: &[
            r"^\[INFO\] Installing environment",
            r"^\[INFO\] Once installed this environment will be reused",
            r"^\[INFO\] This may take a few minutes",
            r"^\s*$",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 40,
        on_empty: "",
    },
    // 34. ps
    CommandFilter {
        command_pattern: r"^ps(\s|$)",
        strip_patterns: &[],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 30,
        on_empty: "",
    },
    // 35. quarto render
    CommandFilter {
        command_pattern: r"^quarto\s+render",
        strip_patterns: &[
            r"^\s*$",
            r"^\s*processing file:",
            r"^\s*\d+/\d+\s",
            r"^\s*running",
            r"^\s*Rendering",
            r"^pandoc ",
            r"^  Validating",
            r"^  Resolving",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "",
    },
    // 36. rsync
    CommandFilter {
        command_pattern: r"^rsync\b",
        strip_patterns: &[r"^\s*$", r"^sending incremental file list", r"^sent \d"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "",
    },
    // 37. shellcheck
    CommandFilter {
        command_pattern: r"^shellcheck\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "",
    },
    // 38. shopify theme push/pull
    CommandFilter {
        command_pattern: r"^shopify\s+theme\s+(push|pull)",
        strip_patterns: &[r"^\s*$", r"^\s*Uploading", r"^\s*Downloading"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 15,
        on_empty: "shopify theme: ok",
    },
    // 39. skopeo
    CommandFilter {
        command_pattern: r"^skopeo\b",
        strip_patterns: &[
            r"^\s*$",
            r"^Getting image source signatures",
            r"^Copying blob",
            r"^Copying config",
            r"^Writing manifest",
            r"^Storing signatures",
        ],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 30,
        on_empty: "skopeo: ok",
    },
    // 40. sops
    CommandFilter {
        command_pattern: r"^sops\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 40,
        on_empty: "",
    },
    // 41. spring-boot (mvn spring-boot:run | java -jar *.jar | gradle bootRun)
    CommandFilter {
        command_pattern: r"^(mvn\s+spring-boot:run|java\s+-jar.*\.jar|gradle\s+.*bootRun)",
        strip_patterns: &[
            // spring-boot uses keep_lines_matching in the TOML; we approximate by
            // stripping everything that does NOT match the keep patterns. Since our
            // engine only supports strip, we strip common Spring banner/startup noise.
            r"^\s*$",
            r"^  \.",       // ASCII art banner lines
            r"^ /\\\\",    // ASCII art banner
            r"^\( \(",     // ASCII art banner
            r"^ \\/",      // ASCII art banner
            r"^  '",        // ASCII art banner
            r"^  :: Spring Boot ::",
            r"^\d{4}-\d{2}-\d{2}.*INFO(?!.*(?:Tomcat started|Started\s|ERROR|WARN|Exception|Caused by|Application run failed|BUILD|Tests run|FAILURE|listening on port))",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "",
    },
    // 42. ssh
    CommandFilter {
        command_pattern: r"^ssh\b",
        strip_patterns: &[
            r"^\s*$",
            r"^Warning: Permanently added",
            r"^Connection to .+ closed",
            r"^Authenticated to",
            r"^debug1:",
            r"^OpenSSH_",
            r"^Pseudo-terminal",
        ],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 200,
        on_empty: "",
    },
    // 43. stat
    CommandFilter {
        command_pattern: r"^stat\b",
        strip_patterns: &[r"^\s*$", r"^\s*Device:", r"^\s*Birth:"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 20,
        on_empty: "",
    },
    // 44. swift build
    CommandFilter {
        command_pattern: r"^swift\s+build\b",
        strip_patterns: &[r"^\s*$", r"^Compiling ", r"^Linking "],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 40,
        on_empty: "",
    },
    // 45. systemctl status
    CommandFilter {
        command_pattern: r"^systemctl\s+status\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "",
    },
    // 46. task (go-task)
    CommandFilter {
        command_pattern: r"^task\b",
        strip_patterns: &[
            r"^\s*$",
            r"^task: \[.*\] ",
            r"^task: Task .* is up to date",
        ],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 50,
        on_empty: "task: ok",
    },
    // 47. terraform plan
    CommandFilter {
        command_pattern: r"^terraform\s+plan",
        strip_patterns: &[
            r"^Refreshing state",
            r"^\s*#.*unchanged",
            r"^\s*$",
            r"^Acquiring state lock",
            r"^Releasing state lock",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 80,
        on_empty: "terraform plan: no changes detected",
    },
    // 48. tofu fmt
    CommandFilter {
        command_pattern: r"^tofu\s+fmt(\s|$)",
        strip_patterns: &[],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "tofu fmt: ok (no changes)",
    },
    // 49. tofu init
    CommandFilter {
        command_pattern: r"^tofu\s+init(\s|$)",
        strip_patterns: &[
            r"^- Downloading",
            r"^- Installing",
            r"^- Using previously-installed",
            r"^\s*$",
            r"^Initializing provider",
            r"^Initializing the backend",
            r"^Initializing modules",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "tofu init: ok",
    },
    // 50. tofu plan
    CommandFilter {
        command_pattern: r"^tofu\s+plan(\s|$)",
        strip_patterns: &[
            r"^Refreshing state",
            r"^\s*#.*unchanged",
            r"^\s*$",
            r"^Acquiring state lock",
            r"^Releasing state lock",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 80,
        on_empty: "tofu plan: no changes detected",
    },
    // 51. tofu validate
    CommandFilter {
        command_pattern: r"^tofu\s+validate(\s|$)",
        strip_patterns: &[],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 0,
        on_empty: "",
    },
    // 52. trunk build
    CommandFilter {
        command_pattern: r"^trunk\s+build",
        strip_patterns: &[
            r"^\s*$",
            r"^\s*Compiling\s",
            r"^\s*Downloading\s",
            r"^\s*Fetching\s",
            r"^\s*Fresh\s",
            r"^\s*Checking\s",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 30,
        on_empty: "trunk build: ok",
    },
    // 53. turbo
    CommandFilter {
        command_pattern: r"^turbo\b",
        strip_patterns: &[
            r"^\s*$",
            r"^\s*cache (hit|miss|bypass)",
            r"^\s*\d+ packages in scope",
            r"^\s*Tasks:\s+\d+",
            r"^\s*Duration:\s+",
            r"^\s*Remote caching (enabled|disabled)",
        ],
        strip_ansi: true,
        max_line_len: 150,
        max_lines: 50,
        on_empty: "turbo: ok",
    },
    // 54. ty
    CommandFilter {
        command_pattern: r"^ty\b",
        strip_patterns: &[r"^\s*$", r"^Checking \d+ file", r"^ty \d+\.\d+"],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 50,
        on_empty: "ty: ok",
    },
    // 55. uv sync / uv pip install
    CommandFilter {
        command_pattern: r"^uv\s+(sync|pip\s+install)\b",
        strip_patterns: &[
            r"^\s*$",
            r"^\s+Downloading ",
            r"^\s+Using cached ",
            r"^\s+Preparing ",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 20,
        on_empty: "",
    },
    // 56. xcodebuild
    CommandFilter {
        command_pattern: r"^xcodebuild\b",
        strip_patterns: &[
            r"^\s*$",
            r"^CompileC\s",
            r"^CompileSwift\s",
            r"^Ld\s",
            r"^CreateBuildDirectory\s",
            r"^MkDir\s",
            r"^ProcessInfoPlistFile\s",
            r"^CopySwiftLibs\s",
            r"^CodeSign\s",
            r"^Signing Identity:",
            r"^RegisterWithLaunchServices",
            r"^Validate\s",
            r"^ProcessProductPackaging",
            r"^Touch\s",
            r"^LinkStoryboards",
            r"^CompileStoryboard",
            r"^CompileAssetCatalog",
            r"^GenerateDSYMFile",
            r"^PhaseScriptExecution",
            r"^PBXCp\s",
            r"^SetMode\s",
            r"^SetOwnerAndGroup\s",
            r"^Ditto\s",
            r"^CpResource\s",
            r"^CpHeader\s",
            r"^\s+cd\s+/",
            r"^\s+export\s",
            r"^\s+/Applications/Xcode",
            r"^\s+/usr/bin/",
            r"^\s+builtin-",
            r"^note: Using new build system",
        ],
        strip_ansi: true,
        max_line_len: 0,
        max_lines: 60,
        on_empty: "xcodebuild: ok",
    },
    // 57. yadm
    CommandFilter {
        command_pattern: r"^yadm\b",
        strip_patterns: &[
            r"^\s*$",
            r#"^\s*\(use "git "#,
            r#"^\s*\(use "yadm "#,
        ],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 40,
        on_empty: "",
    },
    // 58. yamllint
    CommandFilter {
        command_pattern: r"^yamllint\b",
        strip_patterns: &[r"^\s*$"],
        strip_ansi: true,
        max_line_len: 120,
        max_lines: 50,
        on_empty: "",
    },
    // 59. spring-boot (duplicate-safe: #41 already covers the three match patterns)
    // This entry intentionally left as the last; #41's combined regex handles all
    // three Spring Boot invocation styles.
];

// ---------------------------------------------------------------------------
// ANSI stripping
// ---------------------------------------------------------------------------

static ANSI_REGEX: OnceLock<Regex> = OnceLock::new();

fn strip_ansi_codes(s: &str) -> String {
    let re = ANSI_REGEX.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());
    re.replace_all(s, "").into_owned()
}

// ---------------------------------------------------------------------------
// Generic filter engine
// ---------------------------------------------------------------------------

/// Cached compiled regexes for strip patterns.
/// Each entry corresponds to the filter at the same index in FILTERS.
static STRIP_REGEXES: OnceLock<Vec<Vec<Regex>>> = OnceLock::new();

/// Get or initialize the cached strip regexes.
fn get_strip_regexes() -> &'static Vec<Vec<Regex>> {
    STRIP_REGEXES.get_or_init(|| {
        FILTERS
            .iter()
            .map(|f| {
                f.strip_patterns
                    .iter()
                    .filter_map(|p| Regex::new(p).ok())
                    .collect()
            })
            .collect()
    })
}

fn apply_filter(filter: &CommandFilter, filter_idx: usize, output: &str) -> String {
    let output = if filter.strip_ansi {
        strip_ansi_codes(output)
    } else {
        output.to_string()
    };

    // Use cached regexes, falling back to compiling if index is somehow out of bounds
    let strip_regexes: Vec<Regex> = get_strip_regexes()
        .get(filter_idx)
        .cloned()
        .unwrap_or_else(|| {
            filter
                .strip_patterns
                .iter()
                .filter_map(|p| Regex::new(p).ok())
                .collect()
        });

    let lines: Vec<&str> = output
        .lines()
        .filter(|line| !strip_regexes.iter().any(|re| re.is_match(line)))
        .collect();

    // Truncate long lines if needed
    if filter.max_line_len > 0 {
        let truncated: Vec<String> = lines
            .iter()
            .map(|line| {
                if line.len() > filter.max_line_len {
                    format!("{}...", &line[..filter.max_line_len])
                } else {
                    line.to_string()
                }
            })
            .collect();
        let result = cap_lines_owned(&truncated, filter.max_lines);
        if result.is_empty() && !filter.on_empty.is_empty() {
            return filter.on_empty.to_string();
        }
        return result;
    }

    let result = cap_lines(&lines, filter.max_lines);
    if result.is_empty() && !filter.on_empty.is_empty() {
        return filter.on_empty.to_string();
    }
    result
}

fn cap_lines(lines: &[&str], max_lines: usize) -> String {
    if max_lines == 0 || lines.len() <= max_lines {
        return lines.join("\n");
    }
    let first_half = max_lines / 2;
    let last_half = max_lines - first_half;
    let omitted = lines.len() - max_lines;
    let mut parts: Vec<String> = lines[..first_half].iter().map(|s| s.to_string()).collect();
    parts.push(format!("... {} lines omitted", omitted));
    for line in &lines[lines.len() - last_half..] {
        parts.push(line.to_string());
    }
    parts.join("\n")
}

fn cap_lines_owned(lines: &[String], max_lines: usize) -> String {
    if max_lines == 0 || lines.len() <= max_lines {
        return lines.join("\n");
    }
    let first_half = max_lines / 2;
    let last_half = max_lines - first_half;
    let omitted = lines.len() - max_lines;
    let mut parts: Vec<String> = Vec::with_capacity(max_lines + 1);
    parts.extend_from_slice(&lines[..first_half]);
    parts.push(format!("... {} lines omitted", omitted));
    parts.extend_from_slice(&lines[lines.len() - last_half..]);
    parts.join("\n")
}

// ---------------------------------------------------------------------------
// Cargo-specific structural filter
// ---------------------------------------------------------------------------

fn try_filter_cargo(command: &str, output: &str) -> Option<String> {
    let re = Regex::new(r"^cargo\s+(\w+)").ok()?;
    let caps = re.captures(command)?;
    let subcommand = caps.get(1)?.as_str();
    Some(filter_cargo(subcommand, output))
}

fn filter_cargo(subcommand: &str, output: &str) -> String {
    let output = strip_ansi_codes(output);

    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut error_count = 0u32;
    let mut warning_count = 0u32;
    let mut test_summary: Vec<String> = Vec::new();
    let mut other_important: Vec<String> = Vec::new();
    let mut current_block: Vec<String> = Vec::new();
    let mut in_error = false;
    let mut in_warning = false;
    let mut test_pass_count = 0u32;
    let mut test_fail_count = 0u32;
    let mut failures: Vec<String> = Vec::new();
    let mut in_failure_section = false;

    for line in output.lines() {
        let trimmed = line.trim_start();

        // Skip progress/noise lines
        if trimmed.starts_with("Compiling ")
            || trimmed.starts_with("Downloading ")
            || trimmed.starts_with("Downloaded ")
            || trimmed.starts_with("Checking ")
            || trimmed.starts_with("Locking ")
            || trimmed.starts_with("Updating ")
            || trimmed.starts_with("Fresh ")
            || trimmed.starts_with("Packaging ")
            || trimmed.starts_with("Blocking waiting for file lock")
            || trimmed.starts_with("Finished ")
        {
            continue;
        }

        // Detect error blocks
        if trimmed.starts_with("error[") || trimmed.starts_with("error:") {
            flush_block(
                &mut current_block,
                &mut in_error,
                &mut in_warning,
                &mut errors,
                &mut warnings,
                &mut error_count,
                &mut warning_count,
            );
            current_block = vec![line.to_string()];
            in_error = true;
            continue;
        }

        // Detect warning blocks
        if trimmed.starts_with("warning[") || trimmed.starts_with("warning:") {
            flush_block(
                &mut current_block,
                &mut in_error,
                &mut in_warning,
                &mut errors,
                &mut warnings,
                &mut error_count,
                &mut warning_count,
            );
            current_block = vec![line.to_string()];
            in_warning = true;
            continue;
        }

        // Context lines for errors/warnings
        if (in_error || in_warning) && !trimmed.is_empty() {
            current_block.push(line.to_string());
            continue;
        }

        // End of block on empty line
        if (in_error || in_warning) && trimmed.is_empty() {
            flush_block(
                &mut current_block,
                &mut in_error,
                &mut in_warning,
                &mut errors,
                &mut warnings,
                &mut error_count,
                &mut warning_count,
            );
            continue;
        }

        // Test output handling
        if subcommand == "test" || subcommand == "nextest" {
            if trimmed.starts_with("test result:") {
                test_summary.push(line.to_string());
                continue;
            }
            if trimmed.starts_with("test ") && trimmed.ends_with("... ok") {
                test_pass_count += 1;
                continue;
            }
            if trimmed.starts_with("test ") && trimmed.ends_with("... FAILED") {
                test_fail_count += 1;
                failures.push(line.to_string());
                continue;
            }
            if trimmed == "failures:" {
                in_failure_section = true;
                continue;
            }
            if in_failure_section {
                if trimmed.starts_with("test result:") {
                    in_failure_section = false;
                    test_summary.push(line.to_string());
                } else if !trimmed.is_empty() {
                    failures.push(line.to_string());
                }
                continue;
            }
        }

        // Keep other non-noise lines
        if !trimmed.is_empty()
            && !trimmed.starts_with("Running ")
            && !trimmed.starts_with("Doc-tests ")
        {
            other_important.push(line.to_string());
        }
    }

    // Flush last block
    flush_block(
        &mut current_block,
        &mut in_error,
        &mut in_warning,
        &mut errors,
        &mut warnings,
        &mut error_count,
        &mut warning_count,
    );

    // Build compact summary
    let mut parts: Vec<String> = Vec::new();

    if error_count > 0 || warning_count > 0 {
        let mut bits: Vec<String> = Vec::new();
        if error_count > 0 {
            bits.push(format!(
                "{} error{}",
                error_count,
                if error_count == 1 { "" } else { "s" }
            ));
        }
        if warning_count > 0 {
            bits.push(format!(
                "{} warning{}",
                warning_count,
                if warning_count == 1 { "" } else { "s" }
            ));
        }
        parts.push(bits.join(", "));
    }

    for e in &errors {
        parts.push(e.clone());
    }
    for w in warnings.iter().take(5) {
        parts.push(w.clone());
    }
    if warnings.len() > 5 {
        parts.push(format!("... and {} more warnings", warnings.len() - 5));
    }

    if subcommand == "test" || subcommand == "nextest" {
        if test_fail_count > 0 {
            for f in &failures {
                parts.push(f.clone());
            }
        }
        if !test_summary.is_empty() {
            for s in &test_summary {
                parts.push(s.clone());
            }
        } else if test_pass_count > 0 || test_fail_count > 0 {
            parts.push(format!("{} passed, {} failed", test_pass_count, test_fail_count));
        }
    }

    for line in &other_important {
        parts.push(line.clone());
    }

    if parts.is_empty() {
        format!("cargo {}: ok", subcommand)
    } else {
        parts.join("\n")
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_block(
    block: &mut Vec<String>,
    in_error: &mut bool,
    in_warning: &mut bool,
    errors: &mut Vec<String>,
    warnings: &mut Vec<String>,
    error_count: &mut u32,
    warning_count: &mut u32,
) {
    if block.is_empty() {
        *in_error = false;
        *in_warning = false;
        return;
    }
    if *in_error {
        errors.push(block.join("\n"));
        *error_count += 1;
    } else if *in_warning {
        warnings.push(block.join("\n"));
        *warning_count += 1;
    }
    block.clear();
    *in_error = false;
    *in_warning = false;
}

// ---------------------------------------------------------------------------
// Generic fallback: dedup consecutive identical lines, strip ANSI, cap at 100
// ---------------------------------------------------------------------------

fn generic_filter(output: &str) -> String {
    let output = strip_ansi_codes(output);
    let mut result: Vec<String> = Vec::new();
    let mut prev_line: Option<&str> = None;
    let mut dup_count: u32 = 0;

    for line in output.lines() {
        if Some(line) == prev_line {
            dup_count += 1;
        } else {
            if let Some(prev) = prev_line {
                if dup_count > 0 {
                    result.push(format!("{} (x{})", prev, dup_count + 1));
                } else {
                    result.push(prev.to_string());
                }
            }
            prev_line = Some(line);
            dup_count = 0;
        }
    }
    if let Some(prev) = prev_line {
        if dup_count > 0 {
            result.push(format!("{} (x{})", prev, dup_count + 1));
        } else {
            result.push(prev.to_string());
        }
    }

    if result.len() > 100 {
        let omitted = result.len() - 100;
        let mut capped: Vec<String> = Vec::with_capacity(101);
        capped.extend_from_slice(&result[..50]);
        capped.push(format!("... {} lines omitted", omitted));
        capped.extend_from_slice(&result[result.len() - 50..]);
        capped.join("\n")
    } else {
        result.join("\n")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generic_filter_dedup() {
        let output = "line\nline\nline\nother";
        let result = generic_filter(output);
        assert_eq!(result, "line (x3)\nother");
    }

    #[test]
    fn test_strip_ansi() {
        let input = "\x1b[31merror\x1b[0m: something";
        let result = strip_ansi_codes(input);
        assert_eq!(result, "error: something");
    }

    #[test]
    fn test_cargo_build_filters_noise() {
        let output = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\nerror[E0308]: mismatched types\n --> src/main.rs:5:10\n";
        let result = filter("cargo build", output);
        assert!(result.contains("error[E0308]"));
        assert!(!result.contains("Compiling"));
    }

    #[test]
    fn test_make_filter() {
        let output =
            "make[1]: Entering directory '/home/user'\ngcc -O2 foo.c\nmake[1]: Leaving directory '/home/user'\n";
        let result = filter("make all", output);
        assert!(result.contains("gcc -O2 foo.c"));
        assert!(!result.contains("Entering directory"));
    }

    #[test]
    fn test_unknown_command_uses_generic() {
        let output = "a\na\na\nb\n";
        let result = filter("some-unknown-cmd", output);
        assert_eq!(result, "a (x3)\nb");
    }

    #[test]
    fn test_on_empty() {
        let output = "make[1]: Entering directory '/x'\nmake[1]: Leaving directory '/x'\n";
        let result = filter("make", output);
        assert_eq!(result, "make: ok");
    }

    #[test]
    fn test_middle_truncation() {
        let lines: Vec<String> = (0..200).map(|i| format!("line {}", i)).collect();
        let output = lines.join("\n");
        let result = generic_filter(&output);
        assert!(result.contains("... 100 lines omitted"));
    }

    #[test]
    fn test_cargo_test_ok() {
        let output = "   Compiling myapp v0.1.0\ntest tests::it_works ... ok\ntest tests::it_also_works ... ok\n\ntest result: ok. 2 passed; 0 failed; 0 ignored\n";
        let result = filter("cargo test", output);
        assert!(result.contains("test result:"));
        assert!(!result.contains("Compiling"));
    }

    #[test]
    fn test_gradle_strips_up_to_date() {
        let output = "> Configuring project :app\n> Task :app:compileJava UP-TO-DATE\n> Task :app:test\nBUILD FAILED in 12s";
        let result = filter("gradle build", output);
        assert!(result.contains("BUILD FAILED"));
        assert!(!result.contains("UP-TO-DATE"));
        assert!(!result.contains("Configuring project"));
    }

    #[test]
    fn test_gcc_on_empty() {
        let result = filter("gcc -o main main.c", "");
        assert_eq!(result, "gcc: ok");
    }

    #[test]
    fn test_xcodebuild_strips_build_phases() {
        let output = "note: Using new build system\nCompileSwift normal arm64 foo.swift\n    cd /Users/dev\n** BUILD SUCCEEDED **";
        let result = filter("xcodebuild -scheme App", output);
        assert!(result.contains("BUILD SUCCEEDED"));
        assert!(!result.contains("CompileSwift"));
    }

    #[test]
    fn test_ssh_strips_banners() {
        let output = "Warning: Permanently added '10.0.0.1' (ED25519) to the list of known hosts.\n\nuptime: 12:00\nConnection to 10.0.0.1 closed.";
        let result = filter("ssh user@host uptime", output);
        assert!(result.contains("uptime: 12:00"));
        assert!(!result.contains("Permanently added"));
        assert!(!result.contains("Connection to"));
    }

    // -- extract_effective_command -------------------------------------------

    #[test]
    fn test_extract_plain_command() {
        assert_eq!(extract_effective_command("cargo check"), "cargo check");
    }

    #[test]
    fn test_extract_cd_and_cargo() {
        assert_eq!(
            extract_effective_command("cd /tmp/foo && cargo check -p mylib"),
            "cargo check -p mylib"
        );
    }

    #[test]
    fn test_extract_cd_env_cargo() {
        assert_eq!(
            extract_effective_command("cd /tmp/foo && CARGO_BUILD_JOBS=1 cargo check -p mylib"),
            "cargo check -p mylib"
        );
    }

    #[test]
    fn test_extract_timeout_cargo() {
        assert_eq!(
            extract_effective_command("timeout 120 cargo clippy"),
            "cargo clippy"
        );
    }

    #[test]
    fn test_extract_cd_env_timeout_cargo() {
        assert_eq!(
            extract_effective_command("cd /tmp && JOBS=1 timeout 60 cargo build --release"),
            "cargo build --release"
        );
    }

    #[test]
    fn test_cargo_filter_via_cd_wrapper() {
        let output = "   Compiling foo v0.1.0\n   Compiling bar v0.2.0\n    Finished dev [unoptimized] target(s)\n";
        let result = filter("cd /tmp/workdir && CARGO_BUILD_JOBS=1 cargo check -p mylib", output);
        assert_eq!(result, "cargo check: ok");
    }
}
