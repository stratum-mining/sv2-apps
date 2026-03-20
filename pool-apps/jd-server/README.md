## Job Declarator

This is the main orchestrator of the Job Declaration process.

It leverages a `impl JobValidationEngine` and a `TokenManager` to make sure that declared Custom Jobs are managed correctly.

## Job Validation Engine

The goal of the `JobValidationEngine` trait is to allow for Custom Job validation and solution propagation to be done over a modular interface that is agnostic to:
- Bitcoin node implementation
- Communication method with Bitcoin node implementation

The initial implementation is based on Bitcoin Core over IPC, but other approaches should be doable by implementing the `JobValidationEngine` trait.

Please note that token management is not covered here.
More specifically, it is the `JobDeclarator` responsability to leverage a `TokenManager` to manage the tokens to be
added to:
- `AllocateMiningJobToken.Success.mining_job_token`
- `DeclareMiningJob.Success.new_mining_job_token`

Similarly, validation of:
- `DeclareMiningJob.mining_job_token` against `AllocateMiningJobToken.Success.mining_job_token`
- `SetCustomMiningJob.mining_job_token` against `DeclareMiningJob.Success.new_mining_job_token`

are also `JobDeclarator` responsability via `TokenManager`.

## Token Management

todo