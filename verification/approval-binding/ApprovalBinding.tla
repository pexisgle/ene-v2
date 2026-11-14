------------------------------ MODULE ApprovalBinding ------------------------------
\* Model of ene-host's confirm → approve → dispatch → result pipeline.
\*
\* It encodes the guards that keep a device operation on the client generation
\* the user approved it for. Each action mirrors code in apps/ene-host:
\*
\*   ToolCall            permission::gate        (permission/mod.rs)   creates the
\*                       HoldOf call and the Approval bound to the active
\*                       (client, activation_revision) at gate time.
\*   Move                client::activate        (client/mod.rs)       bumps the
\*                       activation revision and expires the pending approvals of
\*                       that companion (db::expire_approvals_for) and fails the
\*                       outstanding operations of the old generation
\*                       (db::fail_operations_of).
\*   SubmitWhenCurrent   permission::submit      (permission/mod.rs)   grants only
\*                       when the approval's generation is still the active one;
\*                       this is defense in depth behind Move's expiry.
\*   SubmitDeny          permission::submit      deny path.
\*   DispatchChecked     client::device::accept  (client/device.rs)    revalidates
\*                       the generation immediately before writing the operation.
\*   ResultSame          client::device::complete (client/device.rs)    applies a
\*                       result only when its generation matches the operation's;
\*                       a late result from another generation is discarded.
\*
\* Scope: one companion, one confirm-gated capability, one tool call. Clients,
\* activation revisions, approvals and operations are the only state. Timeouts,
\* reconnects, work-session bindings and the AUTO policy path are not modelled;
\* see coverage.md.
EXTENDS Naturals, TLC

CONSTANTS Clients, MaxRev
Revs == 0 .. MaxRev

VARIABLES
    active,
    rev,
    callState,
    callClient,
    callRev,
    approvalState,
    approvalClient,
    approvalRev,
    opState,
    opClient,
    opRev,
    dispatchedAt,
    appliedAt

vars == <<active, rev, callState, callClient, callRev,
          approvalState, approvalClient, approvalRev,
          opState, opClient, opRev, dispatchedAt, appliedAt>>

Init ==
    /\ active = "c1"
    /\ rev = 0
    /\ callState = "idle"
    /\ callClient = "c1"
    /\ callRev = 0
    /\ approvalState = "none"
    /\ approvalClient = "c1"
    /\ approvalRev = 0
    /\ opState = "none"
    /\ opClient = "c1"
    /\ opRev = 0
    /\ dispatchedAt = <<"c1", 0>>
    /\ appliedAt = <<"c1", 0>>

\* permission::gate: a confirm-gated capability tool call is accepted against the
\* active client generation and held for the user.
ToolCall ==
    /\ callState = "idle"
    /\ approvalState = "none"
    /\ callState' = "held"
    /\ callClient' = active
    /\ callRev' = rev
    /\ approvalState' = "pending"
    /\ approvalClient' = active
    /\ approvalRev' = rev
    /\ UNCHANGED <<active, rev, opState, opClient, opRev, dispatchedAt, appliedAt>>

\* client::activate: the active client moves, the revision advances, pending
\* approvals of the companion expire and the held call is refused, and the old
\* generation's outstanding operations fail.
Move ==
    /\ rev < MaxRev
    /\ active' \in Clients
    /\ rev' = rev + 1
    /\ IF approvalState = "pending" THEN approvalState' = "expired" ELSE approvalState' = approvalState
    /\ IF callState = "held" THEN callState' = "refused" ELSE callState' = callState
    /\ IF opState \in {"pending", "dispatched"} THEN opState' = "failed" ELSE opState' = opState
    /\ UNCHANGED <<callClient, callRev, approvalClient, approvalRev,
                   opClient, opRev, dispatchedAt, appliedAt>>

\* permission::submit: approve. The generation must still be the active one.
SubmitWhenCurrent ==
    /\ approvalState = "pending"
    /\ callState = "held"
    /\ active = approvalClient
    /\ rev = approvalRev
    /\ approvalState' = "approved"
    /\ callState' = "ready"
    /\ UNCHANGED <<active, rev, callClient, callRev, approvalClient, approvalRev,
                   opState, opClient, opRev, dispatchedAt, appliedAt>>

\* permission::submit: the generation moved; the approval expires and the held
\* call is refused with client_moved.
SubmitAfterMove ==
    /\ approvalState = "pending"
    /\ callState = "held"
    /\ ~(active = approvalClient /\ rev = approvalRev)
    /\ approvalState' = "expired"
    /\ callState' = "refused"
    /\ UNCHANGED <<active, rev, callClient, callRev, approvalClient, approvalRev,
                   opState, opClient, opRev, dispatchedAt, appliedAt>>

SubmitDeny ==
    /\ approvalState = "pending"
    /\ callState = "held"
    /\ approvalState' = "denied"
    /\ callState' = "refused"
    /\ UNCHANGED <<active, rev, callClient, callRev, approvalClient, approvalRev,
                   opState, opClient, opRev, dispatchedAt, appliedAt>>

\* client::device::accept: revalidate the bound generation immediately before
\* writing and dispatching the operation.
DispatchChecked ==
    /\ callState = "ready"
    /\ active = callClient
    /\ rev = callRev
    /\ opClient' = callClient
    /\ opRev' = callRev
    /\ opState' = "dispatched"
    /\ dispatchedAt' = <<active, rev>>
    /\ callState' = "dispatched"
    /\ UNCHANGED <<active, rev, callClient, callRev,
                   approvalState, approvalClient, approvalRev, appliedAt>>

\* client::device::accept: the generation moved before dispatch; the call is
\* refused with client_moved and no operation is written.
DispatchRefused ==
    /\ callState = "ready"
    /\ ~(active = callClient /\ rev = callRev)
    /\ callState' = "refused"
    /\ UNCHANGED <<active, rev, callClient, callRev,
                   approvalState, approvalClient, approvalRev,
                   opState, opClient, opRev, dispatchedAt, appliedAt>>

\* client::device::complete: a result whose client generation matches the
\* operation's is applied.
ResultSame ==
    /\ opState = "dispatched"
    /\ opState' = "done"
    /\ appliedAt' = <<opClient, opRev>>
    /\ UNCHANGED <<active, rev, callState, callClient, callRev,
                   approvalState, approvalClient, approvalRev,
                   opClient, opRev, dispatchedAt>>

\* client::device::complete: a late result from another generation is discarded.
ResultStale ==
    /\ opState = "dispatched"
    /\ UNCHANGED <<active, rev, callState, callClient, callRev,
                   approvalState, approvalClient, approvalRev,
                   opState, opClient, opRev, dispatchedAt, appliedAt>>

Next ==
    \/ ToolCall
    \/ Move
    \/ SubmitWhenCurrent
    \/ SubmitAfterMove
    \/ SubmitDeny
    \/ DispatchChecked
    \/ DispatchRefused
    \/ ResultSame
    \/ ResultStale

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ active \in Clients
    /\ rev \in Revs
    /\ callState \in {"idle", "held", "ready", "dispatched", "done", "refused"}
    /\ callClient \in Clients
    /\ callRev \in Revs
    /\ approvalState \in {"none", "pending", "approved", "denied", "expired"}
    /\ approvalClient \in Clients
    /\ approvalRev \in Revs
    /\ opState \in {"none", "pending", "dispatched", "done", "failed"}
    /\ opClient \in Clients
    /\ opRev \in Revs
    /\ dispatchedAt \in Clients \X Revs
    /\ appliedAt \in Clients \X Revs

\* P1: a pending approval is never left attached to a generation that is no
\* longer active. Move's expiry (db::expire_approvals_for) is what enforces it.
NoStalePendingApproval ==
    approvalState = "pending" => <<approvalClient, approvalRev>> = <<active, rev>>

\* P2: a device operation exists only after the user approved the call.
NoUnapprovedDispatch ==
    opState \in {"pending", "dispatched", "done"} => approvalState = "approved"

\* P3: an operation is dispatched only to the generation that is active at
\* dispatch time (client::device::accept revalidates just before the write).
DispatchedToCurrentGeneration ==
    opState \in {"pending", "dispatched", "done"} => dispatchedAt = <<opClient, opRev>>

\* P4: a result only completes an operation of the generation that produced it
\* (client::device::complete discards a mismatched client or revision).
AppliedBySameGeneration ==
    opState = "done" => appliedAt = <<opClient, opRev>>

=====================================================================================
