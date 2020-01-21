.text

.extern scan_stack
.globl spill_registers

spill_registers:
    // Push all callee-save registers to get them on the stack for conservative
    // stack scanning. We maintain 16-byte alignment at calls (Sys V ABI). There
    // is an 8-byte return address on the stack and we push 56 bytes which
    // maintains 16-byte stack alignment at the call.
    //
    // We don't need to push caller-save registers, as they will have already
    // been spilled if the caller requires them to outlive the call.
    push $0
    push %rbx
    push %rbp
    push %r12
    push %r13
    push %r14
    push %r15
    // %rsi contains the function pointer to our stack scanning code. We move it
    // to %r8 (a scratch register) so that the marking context pointer can be
    // placed in %rdi as the first arg slot.
    mov %rsi, %r8
    mov %rsp, %rsi
    call *%r8
    // Pop all the callee-save registers (ret will pop the return addr)
    add $56, %rsp
    ret

