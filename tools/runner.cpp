// tools/runner.cpp
//
// The "runner" is a small C++ program used as the `builder` of every REAPI
// action derivation that nixception creates.  It replaces the previous bash
// wrapper script for efficiency: instead of generating a shell script that
// spawns mkdir/cp/tree for every input file, this program performs the same
// operations directly via system calls and std::filesystem.
//
// Data flow (Nix passAsFile mechanism)
// ────────────────────────────────────
// The Nix derivation environment contains:
//
//   passAsFile = "manifest"
//   manifest   = <JSON content>
//   out        = /nix/store/…-reapi-action
//
// When the Nix daemon builds the derivation it writes the manifest content to
// a temporary file and sets $manifestPath to its location.  The runner reads
// that file, parses the JSON manifest, and executes the described action.
//
// Manifest schema (JSON)
// ──────────────────────
// {
//   "inputs": [
//     {"store_path": "/nix/store/…", "path": "./relative/path"},
//     …
//   ],
//   "working_directory": ".",
//   "output_directories": ["dir1", …],
//   "output_files":       ["file1", …],
//   "output_paths":       ["path1", …],
//   "environment":        {"VAR": "value", …},
//   "command":            ["prog", "arg1", …],
//   "path":               "/nix/store/…-coreutils/bin:/nix/store/…-gcc/bin:…"
// }
//
// Execution steps
// ───────────────
//  1. Read $manifestPath → parse JSON.
//  2. For each input: create parent directories, copy file from store, make
//     writable (store originals are read-only).
//  3. Create working directory and output directory structure.
//  4. Create $out/outputs/.
//  5. fork():
//     • Child: chdir to working_directory, set environment variables (incl.
//       PATH from manifest), redirect stdout/stderr to $out/stdout and
//       $out/stderr, execvp() the command.
//     • Parent: waitpid(), write exit code to $out/exitcode.
//  6. Copy outputs into $out/outputs/ preserving directory structure.
//     Missing outputs are silently skipped (the real exit code is already
//     saved; failing here would hide it).
//
// Error handling
// ──────────────
// Every infrastructure error (cannot read manifest, cannot copy inputs,
// cannot fork, etc.) is fatal: the runner prints a diagnostic to stderr and
// exits with a non-zero code so that Nix marks the derivation as failed.
// The only non-fatal condition is missing output files/directories after the
// command has run — those are silently skipped and will be dealt with by the
// caller (nixception's collect_action_result).
//
// Build
// ─────
// Built by runner.nix via:
//   $CXX -std=c++17 -O2 -o runner runner.cpp -I${nlohmann_json}/include
//
// The only external dependency is nlohmann/json (header-only).

#include <nlohmann/json.hpp>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <string>
#include <utility>
#include <vector>

#include <fcntl.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace fs = std::filesystem;
using json = nlohmann::json;

// ── helpers ─────────────────────────────────────────────────────────────────

/// Print a prefixed error message to stderr and exit with the given code.
/// This function never returns.
[[noreturn]] static void die(const std::string &msg, int code = 1) {
    std::cerr << "runner: FATAL: " << msg << std::endl;
    std::exit(code);
}

/// Recursively list the directory tree rooted at `dir` to stderr, for
/// debugging.  Loosely mimics `tree` output.
static void print_tree(const fs::path &dir, const std::string &prefix = "") {
    std::error_code ec;
    if (!fs::exists(dir, ec)) {
        std::cerr << prefix << dir.string() << " [does not exist]" << std::endl;
        return;
    }
    std::cerr << prefix << dir.string() << "/" << std::endl;
    for (auto it = fs::recursive_directory_iterator(
             dir, fs::directory_options::follow_directory_symlink |
                      fs::directory_options::skip_permission_denied,
             ec);
         !ec && it != fs::recursive_directory_iterator(); it.increment(ec)) {
        std::string indent(static_cast<size_t>(it.depth() + 1) * 2, ' ');
        const auto &entry = *it;
        if (entry.is_directory(ec)) {
            std::cerr << prefix << indent << entry.path().filename().string()
                      << "/" << std::endl;
        } else {
            std::cerr << prefix << indent << entry.path().filename().string()
                      << std::endl;
        }
    }
}

// ── manifest & environment reading ──────────────────────────────────────────

/// Read and parse the JSON manifest from the path given by $manifestPath.
/// Dies on any failure.
static json read_manifest() {
    const char *manifest_path_env = std::getenv("manifestPath");
    if (!manifest_path_env || manifest_path_env[0] == '\0') {
        die("$manifestPath is not set "
            "(is passAsFile = \"manifest\" in the derivation?)");
    }

    std::ifstream manifest_file(manifest_path_env);
    if (!manifest_file) {
        die(std::string("cannot open manifest file: ") + manifest_path_env);
    }

    try {
        return json::parse(manifest_file);
    } catch (const json::parse_error &e) {
        die(std::string("JSON parse error in manifest: ") + e.what());
    }
}

/// Read the $out environment variable.  Dies if not set.
static fs::path get_out_dir() {
    const char *out_dir_env = std::getenv("out");
    if (!out_dir_env || out_dir_env[0] == '\0') {
        die("$out is not set");
    }
    return fs::path(out_dir_env);
}

// ── input staging ───────────────────────────────────────────────────────────

/// Copy all input files from the Nix store into the build directory,
/// preserving their relative paths and making them writable.
/// Dies on any copy failure.
static void copy_inputs(const json &manifest) {
    if (!manifest.contains("inputs") || !manifest["inputs"].is_array()) {
        return;
    }

    for (const auto &input : manifest["inputs"]) {
        fs::path target(input.at("path").get<std::string>());
        fs::path source(input.at("store_path").get<std::string>());

        std::error_code ec;
        fs::create_directories(target.parent_path(), ec);
        if (ec) {
            die("cannot create parent directory for input '"
                + target.string() + "': " + ec.message());
        }

        fs::copy_file(source, target,
                      fs::copy_options::overwrite_existing, ec);
        if (ec) {
            die("cannot copy input " + source.string()
                + " -> " + target.string() + ": " + ec.message());
        }

        // Make the copy writable (store originals are read-only).
        fs::permissions(target,
                        fs::perms::owner_read | fs::perms::owner_write,
                        fs::perm_options::add, ec);
        if (ec) {
            die("cannot chmod input '" + target.string()
                + "': " + ec.message());
        }
    }
}

// ── working directory ───────────────────────────────────────────────────────

/// Create and chdir into the working directory specified in the manifest.
/// An empty or "." working_directory means "stay in the current directory".
/// Returns the effective working directory path.  Dies on failure.
static std::string setup_working_directory(const json &manifest) {
    std::string cwd = ".";
    if (manifest.contains("working_directory")) {
        std::string val = manifest["working_directory"].get<std::string>();
        if (!val.empty()) {
            cwd = val;
        }
    }

    if (cwd != ".") {
        std::error_code ec;
        fs::create_directories(cwd, ec);
        if (ec) {
            die("cannot create working directory '" + cwd
                + "': " + ec.message());
        }
        if (::chdir(cwd.c_str()) != 0) {
            die("chdir('" + cwd + "'): " + std::strerror(errno));
        }
    }

    return cwd;
}

// ── command output pre-creation ─────────────────────────────────────────────

/// Pre-create the directory structure expected by the command's declared
/// outputs.  For output_directories we create the directory itself; for
/// output_files and output_paths we create the parent directory.
/// Dies on failure.
static void prepare_command_outputs(const json &manifest) {
    std::error_code ec;

    auto mkdir_parents = [&](const std::string &p) {
        fs::create_directories(p, ec);
        if (ec) {
            die("cannot create output directory '" + p
                + "': " + ec.message());
        }
    };

    auto mkdir_file_parent = [&](const std::string &p) {
        auto parent = fs::path(p).parent_path();
        if (parent.empty()) return;
        fs::create_directories(parent, ec);
        if (ec) {
            die("cannot create parent directory for output '"
                + p + "': " + ec.message());
        }
    };

    if (manifest.contains("output_directories")) {
        for (const auto &d : manifest["output_directories"]) {
            mkdir_parents(d.get<std::string>());
        }
    }
    if (manifest.contains("output_files")) {
        for (const auto &f : manifest["output_files"]) {
            mkdir_file_parent(f.get<std::string>());
        }
    }
    if (manifest.contains("output_paths")) {
        for (const auto &p : manifest["output_paths"]) {
            mkdir_file_parent(p.get<std::string>());
        }
    }
}

/// Create the execution-result directory ($out/outputs/) where the runner
/// stores stdout, stderr, exitcode, and collected command outputs.
/// Dies on failure.
static void create_result_dir(const fs::path &out_dir) {
    std::error_code ec;
    fs::create_directories(out_dir / "outputs", ec);
    if (ec) {
        die("cannot create $out/outputs: " + ec.message());
    }
}

// ── child process environment & argv ────────────────────────────────────────

/// Owns a list of strings and exposes them as a null-terminated
/// `const char*` array suitable for execvpe / environ.
///
/// Usage:
///   CStringArray arr;
///   arr.push_back("hello");
///   arr.push_back("world");
///   const char *const *p = arr.data();  // {"hello","world",nullptr}
///
/// `data()` lazily rebuilds the pointer array whenever new strings have
/// been pushed since the last call, so callers never see stale pointers.
class CStringArray {
public:
    void push_back(std::string s) {
        storage_.push_back(std::move(s));
        dirty_ = true;
    }

    /// Return a null-terminated array of C strings.
    /// The pointers are valid until the next push_back() or destruction.
    const char *const *data() const {
        if (dirty_) {
            finalize();
        }
        return ptrs_.data();
    }

private:
    void finalize() const {
        ptrs_.clear();
        for (const auto &s : storage_) {
            ptrs_.push_back(s.c_str());
        }
        ptrs_.push_back(nullptr);
        dirty_ = false;
    }

    std::vector<std::string> storage_;
    mutable std::vector<const char *> ptrs_;  // null-terminated
    mutable bool dirty_ = true;
};

/// Build the environment variable array from the manifest's "environment"
/// object.  The REAPI environment is passed through verbatim.
static CStringArray build_child_env(const json &manifest) {
    CStringArray env;
    if (manifest.contains("environment") &&
        manifest["environment"].is_object()) {
        for (auto it = manifest["environment"].begin();
             it != manifest["environment"].end(); ++it) {
            env.push_back(it.key() + "=" +
                          it.value().get_ref<const std::string &>());
        }
    }
    return env;
}

/// Build the argv array from the manifest's "command" array.
/// Dies if the command is missing or empty.
static CStringArray build_child_argv(const json &manifest) {
    if (!manifest.contains("command") || !manifest["command"].is_array() ||
        manifest["command"].empty()) {
        die("manifest has no 'command' array or it is empty");
    }

    CStringArray cmd;
    for (const auto &arg : manifest["command"]) {
        cmd.push_back(arg.get<std::string>());
    }
    return cmd;
}

// ── command execution ───────────────────────────────────────────────────────

/// Fork and exec the command, redirecting stdout/stderr into $out,
/// then wait for the child and write the exit code to $out/exitcode.
/// Returns the child's exit code.  Dies on infrastructure failures
/// (fork, waitpid, file creation).
static int execute_command(const CStringArray &cmd,
                           const CStringArray &env,
                           const fs::path &out_dir) {
    const fs::path stdout_path = out_dir / "stdout";
    const fs::path stderr_path = out_dir / "stderr";
    const fs::path exitcode_path = out_dir / "exitcode";

    pid_t pid = ::fork();
    if (pid < 0) {
        die(std::string("fork: ") + std::strerror(errno));
    }

    if (pid == 0) {
        // ── Child process ───────────────────────────────────────────────
        // Redirect stdout → $out/stdout
        int fd_out =
            ::open(stdout_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd_out < 0) {
            dprintf(STDERR_FILENO, "runner: child: cannot open %s: %s\n",
                    stdout_path.c_str(), std::strerror(errno));
            ::_exit(126);
        }
        ::dup2(fd_out, STDOUT_FILENO);
        ::close(fd_out);

        // Redirect stderr → $out/stderr
        int fd_err =
            ::open(stderr_path.c_str(), O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd_err < 0) {
            // stdout is already redirected, write to original stderr fd
            // (which Nix captures).  Best-effort.
            dprintf(STDERR_FILENO, "runner: child: cannot open %s: %s\n",
                    stderr_path.c_str(), std::strerror(errno));
            ::_exit(126);
        }
        ::dup2(fd_err, STDERR_FILENO);
        ::close(fd_err);

        // execve with the REAPI environment.
        ::execvpe(cmd.data()[0],
                  const_cast<char *const *>(cmd.data()),
                  const_cast<char *const *>(env.data()));

        // If execvpe returns, it failed.
        dprintf(STDERR_FILENO, "runner: execvpe(%s): %s\n", cmd.data()[0],
                std::strerror(errno));
        ::_exit(127);
    }

    // ── Parent process ──────────────────────────────────────────────────
    int status = 0;
    while (::waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR) {
            die(std::string("waitpid: ") + std::strerror(errno));
        }
    }

    int exit_code;
    if (WIFEXITED(status)) {
        exit_code = WEXITSTATUS(status);
    } else if (WIFSIGNALED(status)) {
        exit_code = 128 + WTERMSIG(status);
    } else {
        exit_code = 1;
    }

    // Write exit code to $out/exitcode.
    {
        std::ofstream ofs(exitcode_path.string());
        if (!ofs) {
            die("cannot write " + exitcode_path.string());
        }
        ofs << exit_code << "\n";
    }

    // Ensure stdout/stderr files exist even if the child never wrote to
    // them (the open() in the child should have created them, but if
    // fork ran into trouble they might be missing).
    {
        std::error_code ec;
        if (!fs::exists(stdout_path, ec)) {
            std::ofstream touch(stdout_path.string());
            if (!touch) {
                die("cannot create " + stdout_path.string());
            }
        }
        if (!fs::exists(stderr_path, ec)) {
            std::ofstream touch(stderr_path.string());
            if (!touch) {
                die("cannot create " + stderr_path.string());
            }
        }
    }

    return exit_code;
}

// ── command output collection ───────────────────────────────────────────────

/// Recursively copy `src` into `dest_root`, preserving relative directory
/// structure (analogous to `cp --parents -r src dest_root`).
///
///   copy_with_parents("build/lib", "/nix/store/…/outputs")
///     → copies build/lib/… to /nix/store/…/outputs/build/lib/…
///
/// Missing source files are silently skipped — the command may not have
/// produced all declared outputs.  Actual copy errors are fatal.
static void copy_with_parents(const fs::path &src,
                              const fs::path &dest_root) {
    std::error_code ec;

    if (!fs::exists(src, ec)) {
        // Missing output — just skip. Not ours to deal with.
        // It pertains to the consumer to know what to do with that (fail, restry, etc.)
        return;
    }

    fs::path dest = dest_root / src;

    if (fs::is_directory(src, ec)) {
        fs::create_directories(dest, ec);
        if (ec) {
            die("cannot create output destination directory '"
                + dest.string() + "': " + ec.message());
        }
        fs::copy(src, dest,
                 fs::copy_options::recursive |
                     fs::copy_options::copy_symlinks,
                 ec);
        if (ec) {
            die("cannot copy output directory '" + src.string()
                + "' -> '" + dest.string() + "': " + ec.message());
        }
    } else {
        fs::create_directories(dest.parent_path(), ec);
        if (ec) {
            die("cannot create parent for output file '"
                + dest.string() + "': " + ec.message());
        }
        fs::copy_file(src, dest, fs::copy_options::none, ec);
        if (ec) {
            die("cannot copy output file '" + src.string()
                + "' -> '" + dest.string() + "': " + ec.message());
        }
    }
}

/// Copy all declared command outputs into $out/outputs/, preserving
/// directory structure.  Missing outputs are silently skipped.
///
/// Per the REAPI v2.1 spec (Command message):
///   "If `output_paths` is used, `output_files` and `output_directories`
///    will be ignored!"
/// So when `output_paths` is present and non-empty we use it exclusively;
/// otherwise we fall back to the deprecated `output_files` +
/// `output_directories` fields.
static void collect_command_outputs(const json &manifest, const fs::path &out_dir) {
    const fs::path outputs_dir = out_dir / "outputs";

    bool has_output_paths = manifest.contains("output_paths")
                            && !manifest["output_paths"].empty();

    if (has_output_paths) {
        // v2.1+: unified output_paths supersedes the legacy fields.
        for (const auto &p : manifest["output_paths"]) {
            copy_with_parents(fs::path(p.get<std::string>()), outputs_dir);
        }
    } else {
        // Legacy: separate output_files and output_directories.
        if (manifest.contains("output_directories")) {
            for (const auto &d : manifest["output_directories"]) {
                copy_with_parents(fs::path(d.get<std::string>()), outputs_dir);
            }
        }
        if (manifest.contains("output_files")) {
            for (const auto &f : manifest["output_files"]) {
                copy_with_parents(fs::path(f.get<std::string>()), outputs_dir);
            }
        }
    }
}

// ── main ────────────────────────────────────────────────────────────────────

int main() {
    // 1. Read manifest and $out.
    json manifest = read_manifest();
    fs::path out_dir = get_out_dir();

    // 2. Copy input files from the Nix store into the build sandbox.
    copy_inputs(manifest);
    print_tree(".");

    // 3. Set up working directory, pre-create command output dirs,
    //    and create the execution-result directory.
    setup_working_directory(manifest);
    prepare_command_outputs(manifest);
    create_result_dir(out_dir);

    // 4. Prepare child process environment and argv.
    CStringArray env = build_child_env(manifest);
    CStringArray cmd = build_child_argv(manifest);

    // 5. Fork, exec, wait.  The child's exit code is written to
    //    $out/exitcode; the runner itself only fails on infrastructure
    //    errors, not on command failure.
    execute_command(cmd, env, out_dir);
    print_tree(".");

    // 6. Collect declared command outputs into $out/outputs/.
    //    Missing outputs are silently skipped.
    collect_command_outputs(manifest, out_dir);
    print_tree(out_dir);

    // The runner itself always exits 0.  The action's real exit code is
    // recorded in $out/exitcode and interpreted by nixception's
    // collect_action_result().
    return 0;
}
