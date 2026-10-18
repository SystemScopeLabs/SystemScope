//! Runtime errors say what failed.

use systemscope_runtime::runtime::RuntimeError;
use systemscope_runtime::scheduler::SchedulerRestoreError;
use systemscope_runtime::topology::ElaborationError;
use systemscope_runtime::trace::ResumeError;

#[test]
fn runtime_errors_say_what_failed() {
    assert_eq!(
        RuntimeError::TraceNeedsPrefix.to_string(),
        "a restored session resumes its trace with resume_trace"
    );
    assert_eq!(
        SchedulerRestoreError::DuplicateSequence(3).to_string(),
        "duplicate sequence 3"
    );
    assert_eq!(
        ElaborationError::UnknownPort("cpu.mem".to_owned()).to_string(),
        "unknown port cpu.mem"
    );
    assert_eq!(
        ResumeError::HeaderMismatch.to_string(),
        "trace header differs from the session"
    );
}
