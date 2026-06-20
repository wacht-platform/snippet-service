# sandbox_environment

[workspace]
root = "configured workspace path"
writable = true
state = "configured state_path stores durable harness state"
boundary = "all file tools must stay inside the configured workspace"

[paths]
source = "repository and working files"
state_path = "durable harness state, not source code"
scratch = "temporary files only when they serve the current coding request"

[commands]
cwd = "configured workspace path"
role = "inspect, build, test, format, and verify"
output = "keep command output narrow enough to be useful"
missing_binary = "adapt to available tools or report the blocker in complete"
failure = "read stdout/stderr and act on the concrete error"
