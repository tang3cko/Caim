---@meta caim
-- Type definitions for the caim table that `caim script` gives a Lua script, for the Lua language
-- server (LuaLS). `caim script --types` prints this file; it describes the caim that printed it.

---@class caim.Verdict
local Verdict = {}

---Records a case's verdict; a case recorded twice keeps the worse of the two (VOID over FAIL over
---PASS), and a verdict other than the three is VOID.
---@param verdict "PASS"|"FAIL"|"VOID"|string
---@param name string
---@return caim.Verdict
function Verdict:record(verdict, name) end

---@param name string
---@return caim.Verdict
function Verdict:pass(name) end

---@param name string
---@return caim.Verdict
function Verdict:fail(name) end

---@param name string
---@return caim.Verdict
function Verdict:void(name) end

---Names cases that must be recorded; one never recorded is VOID.
---@param names string[]
---@return caim.Verdict
function Verdict:expect(names) end

---Prints a `VERDICT X case=NAME` line per case and a total line, and returns the worst verdict as
---a status: 0 for PASS, 1 for FAIL, 2 for VOID. No case at all is VOID.
---@return 0|1|2
function Verdict:finish() end

---@class caim.SpawnSpec
---@field [integer] string The command and its arguments
---@field output? string A file for the process's stdout and stderr
---@field env? table<string, string> Variables added to the environment

---@class caim.RunSpec
---@field [integer] string The command and its arguments
---@field output string The file for the command's output, which the stall guard watches
---@field idle number Seconds without progress after which the command is stopped
---@field steps? string[] Stop steps as `SIGNAL:SECONDS`, `{ "TERM:20", "KILL:20" }` by default
---@field env? table<string, string> Variables added to the environment

---@class caim.Stale
---@field list string The stamp
---@field path? string The file that changed or is missing; nil when the stamp itself does not exist

---@class caim
caim = {}

---The task's arguments, from `CAIM_ARG_<NAME>` with the names lower-cased.
---@type table<string, string>
caim.args = {}

---The name of the task running the script, from `CAIM_TASK`.
---@type string
caim.task = ""

---The project's root: `CAIM_ROOT`, else the nearest directory with a caim.toml, else the current one.
---@type string
caim.root = ""

---Starts a process with no stdin and returns its pid.
---@param spec caim.SpawnSpec
---@return integer pid
function caim.spawn(spec) end

---Waits for a process this script started and returns its status, 128 plus the signal when a
---signal ended it; nil once it has been waited for.
---@param pid integer
---@return integer? status
function caim.exit(pid) end

---Waits for the first of several processes this script started to exit and returns its pid.
---@param pids integer[]
---@return integer pid
function caim.first(pids) end

---Waits for a line of the file to match the regular expression: true, or false when `pid` exited
---first without one. A file that does not exist yet is waited for.
---@param path string
---@param pattern string A Rust regular expression
---@param pid? integer
---@return boolean matched
function caim.line(path, pattern, pid) end

---Starts a coroutine and returns its id; it runs when the script waits.
---@param fn fun(): ...
---@return integer id
function caim.go(fn) end

---Waits for a coroutine and returns what it returned.
---@param id integer
---@return ...
function caim.join(id) end

---Declares a progress source to the caim running the task; false when there is none to tell.
---@param kind "log"|"dir"
---@param path string
---@return boolean
function caim.declare(kind, path) end

---Prints the message to stderr and ends the script with 64, from any coroutine.
---@param message string
---@return nil
function caim.usage(message) end

---The words of a command line: `"--option", value` for each argument that is not empty, in the
---order of the argument names.
---@param args table<string, string>
---@param options table<string, string> Argument name to option, such as `{ profile = "--profile" }`
---@return string[]
function caim.flags(args, options) end

---The argument NAME, else the environment variable NAME upper-cased with `-` as `_`, else the
---default. An empty value counts as none.
---@param name string
---@param default? string
---@return string?
function caim.env(name, default) end

---The path made absolute against the current directory, with `.` and `..` resolved as text.
---@param path string
---@return string
function caim.abs(path) end

---Stops a process with `SIGNAL:SECONDS` steps: true when it is gone, false when it outlived them.
---@param pid integer
---@param steps? string[] `{ "TERM:20", "KILL:20" }` by default
---@return boolean gone
function caim.stop(pid, steps) end

---Runs a command to its end under a stall guard. Every coroutine of the script waits while it runs.
---@param spec caim.RunSpec
---@return "exit"|"stall"|"interrupted"|"unavailable" ended
---@return integer? status The exit status, or the signal when interrupted
function caim.run(spec) end

---Runs the function when the script ends, last registered first, whatever ended it. It cannot wait.
---@param fn fun()
function caim.defer(fn) end

---Iterates over the lines of the file that match the regular expression, giving the line and then
---each capture group. A file that does not exist has none.
---@param path string
---@param pattern string A Rust regular expression
---@return fun(): string?, ...: string
function caim.lines(path, pattern) end

---A collector of case verdicts.
---@return caim.Verdict
function caim.verdict() end

---The `shasum -a 256` lines for the files; nil and the reason when one cannot be read.
---@param paths string[]
---@return string? lines
---@return string? reason
function caim.hash(paths) end

---Checks files against stamps: true and an empty list when none changed, else false and a list of
---what is stale.
---@param stamps string[]
---@return boolean fresh
---@return caim.Stale[] stale
function caim.fresh(stamps) end

---Runs another task of the project, as `caim TASK --name=value` would, waits for it, and returns its
---status. A value is a string or number; true gives a flag and false nothing.
---@param task string
---@param args? table<string, string|number|boolean>
---@return integer? status
function caim.invoke(task, args) end
