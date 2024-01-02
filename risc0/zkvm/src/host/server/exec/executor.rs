// Copyright 2024 RISC Zero, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module implements the Executor.

use std::{cell::RefCell, fmt::Debug, io::Write, mem, rc::Rc};

use addr2line::{
    fallible_iterator::FallibleIterator,
    gimli::{EndianRcSlice, RunTimeEndian},
    Frame, LookupResult, ObjectContext,
};
use anyhow::{anyhow, bail, Context, Result};
use crypto_bigint::{CheckedMul, Encoding, NonZero, U256, U512};
use risc0_binfmt::{MemoryImage, Program, SystemState};
use risc0_zkp::{
    core::{
        digest::{DIGEST_BYTES, DIGEST_WORDS},
        hash::sha::{BLOCK_BYTES, BLOCK_WORDS},
        log2_ceil,
    },
    MAX_CYCLES_PO2, MIN_CYCLES_PO2, ZK_CYCLES,
};
use risc0_zkvm_platform::{
    fileno,
    memory::{is_guest_memory, GUEST_MAX_MEM},
    syscall::{
        bigint, ecall, halt,
        reg_abi::{REG_A0, REG_A1, REG_A2, REG_A3, REG_A4, REG_T0},
    },
    PAGE_SIZE, WORD_SIZE,
};
use rrs_lib::{instruction_executor::InstructionExecutor, HartState};
use serde::{Deserialize, Serialize};
use sha2::digest::generic_array::GenericArray;
use tempfile::tempdir;
use tracing::{level_filters::LevelFilter, Level};

use super::{monitor::MemoryMonitor, profiler::Profiler, syscall::SyscallTable};
use crate::{
    align_up,
    host::{
        client::exec::TraceEvent,
        receipt::Assumption,
        server::opcode::{MajorType, OpCode},
    },
    sha::Digest,
    Assumptions, ExecutorEnv, ExitCode, FileSegmentRef, Loader, Output, Segment, SegmentRef,
    Session,
};

/// The number of cycles required to compress a SHA-256 block.
const SHA_CYCLES: usize = 73;

/// Number of cycles required to complete a BigInt operation.
const BIGINT_CYCLES: usize = 9;

/// The default segment limit specified in powers of 2 cycles. Choose this value
/// to try and fit with 8GB of RAM.
const DEFAULT_SEGMENT_LIMIT_PO2: u32 = 20; // 1M cycles

// Capture the journal output in a buffer that we can access afterwards.
#[derive(Clone, Default)]
struct Journal {
    buf: Rc<RefCell<Vec<u8>>>,
}

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buf.borrow_mut().write(bytes)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.buf.borrow_mut().flush()
    }
}

#[derive(Clone)]
struct OpCodeResult {
    pc: u32,
    exit_code: Option<ExitCode>,
    extra_cycles: usize,
}

impl OpCodeResult {
    fn new(pc: u32, exit_code: Option<ExitCode>, extra_cycles: usize) -> Self {
        Self {
            pc,
            exit_code,
            extra_cycles,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyscallRecord {
    pub to_guest: Vec<u32>,
    pub regs: (u32, u32),
}

/// The Executor provides an implementation for the execution phase.
///
/// The proving phase uses an execution trace generated by the Executor.
pub struct ExecutorImpl<'a> {
    env: ExecutorEnv<'a>,
    pub(crate) syscall_table: SyscallTable<'a>,
    pre_image: Option<Box<MemoryImage>>,
    monitor: MemoryMonitor,
    pc: u32,
    init_cycles: usize,
    body_cycles: usize,
    segment_limit: usize,
    segment_cycle: usize,
    segments: Vec<Box<dyn SegmentRef>>,
    insn_counter: u32,
    split_insn: Option<u32>,
    const_cycles: usize,
    pending_syscall: Option<SyscallRecord>,
    syscalls: Vec<SyscallRecord>,
    exit_code: Option<ExitCode>,
    obj_ctx: Option<ObjectContext>,
    output_digest: Option<Digest>,
    profiler: Option<Rc<RefCell<Profiler>>>,
}

impl<'a> ExecutorImpl<'a> {
    /// Construct a new [ExecutorImpl] from a [MemoryImage] and entry point.
    ///
    /// Before a guest program is proven, the [ExecutorImpl] is responsible for
    /// deciding where a zkVM program should be split into [Segment]s and what
    /// work will be done in each segment. This is the execution phase:
    /// the guest program is executed to determine how its proof should be
    /// divided into subparts.
    pub fn new(env: ExecutorEnv<'a>, image: MemoryImage) -> Result<Self> {
        Self::with_details(env, image, None, None)
    }

    fn with_details(
        env: ExecutorEnv<'a>,
        image: MemoryImage,
        obj_ctx: Option<ObjectContext>,
        profiler: Option<Rc<RefCell<Profiler>>>,
    ) -> Result<Self> {
        // Enforce segment_limit_po2 bounds
        let segment_limit_po2 = env.segment_limit_po2.unwrap_or(DEFAULT_SEGMENT_LIMIT_PO2) as usize;
        if segment_limit_po2 < MIN_CYCLES_PO2 || segment_limit_po2 > MAX_CYCLES_PO2 {
            bail!("Invalid segment_limit_po2: {}", segment_limit_po2);
        }

        let pc = image.pc;
        let monitor = MemoryMonitor::new(image.clone(), !env.trace.is_empty());
        let loader = Loader::new();
        let init_cycles = loader.init_cycles();
        let fini_cycles = loader.fini_cycles();
        let const_cycles = init_cycles + fini_cycles + SHA_CYCLES + ZK_CYCLES;
        let syscall_table = SyscallTable::new(&env);

        Ok(Self {
            env,
            syscall_table,
            pre_image: Some(Box::new(image)),
            monitor,
            pc,
            init_cycles,
            body_cycles: 0,
            segment_limit: 1 << segment_limit_po2,
            segment_cycle: init_cycles,
            segments: Vec::new(),
            insn_counter: 0,
            split_insn: None,
            const_cycles,
            pending_syscall: None,
            syscalls: Vec::new(),
            exit_code: None,
            obj_ctx,
            output_digest: None,
            profiler,
        })
    }

    /// Construct a new [ExecutorImpl] from the ELF binary of the guest program
    /// you want to run and an [ExecutorEnv] containing relevant
    /// environmental configuration details.
    ///
    /// # Example
    /// ```
    /// use risc0_zkvm::{ExecutorImpl, ExecutorEnv, Session};
    /// use risc0_zkvm_methods::{BENCH_ELF, bench::{BenchmarkSpec, SpecWithIters}};
    ///
    /// let env = ExecutorEnv::builder()
    ///     .write(&SpecWithIters(BenchmarkSpec::SimpleLoop, 1))
    ///     .unwrap()
    ///     .build()
    ///     .unwrap();
    /// let mut exec = ExecutorImpl::from_elf(env, BENCH_ELF).unwrap();
    /// ```
    pub fn from_elf(mut env: ExecutorEnv<'a>, elf: &[u8]) -> Result<Self> {
        let program = Program::load_elf(elf, GUEST_MAX_MEM as u32)?;
        let image = MemoryImage::new(&program, PAGE_SIZE as u32)?;

        let obj_ctx = if LevelFilter::current().eq(&Level::TRACE) {
            let file = addr2line::object::read::File::parse(elf)?;
            Some(ObjectContext::new(&file)?)
        } else {
            None
        };

        let profiler = if env.pprof_out.is_some() {
            let profiler = Rc::new(RefCell::new(Profiler::new(elf, None)?));
            env.trace.push(profiler.clone());
            Some(profiler)
        } else {
            None
        };

        Self::with_details(env, image, obj_ctx, profiler)
    }

    /// This will run the executor to get a [Session] which contain the results
    /// of the execution.
    pub fn run(&mut self) -> Result<Session> {
        if self.env.segment_path.is_none() {
            self.env.segment_path = Some(tempdir()?.into_path());
        }

        let path = self.env.segment_path.clone().unwrap();
        self.run_with_callback(|segment| Ok(Box::new(FileSegmentRef::new(&segment, &path)?)))
    }

    /// Run the executor until [ExitCode::Halted], [ExitCode::Paused], or
    /// [ExitCode::Fault] is reached, producing a [Session] as a result.
    pub fn run_with_callback<F>(&mut self, mut callback: F) -> Result<Session>
    where
        F: FnMut(Segment) -> Result<Box<dyn SegmentRef>>,
    {
        let (Some(ExitCode::SystemSplit | ExitCode::Paused(_)) | None) = self.exit_code else {
            return Err(anyhow!(
                "cannot resume an execution which exited with {:?}",
                self.exit_code
            )
            .into());
        };

        let start_time = std::time::Instant::now();

        self.pc = self
            .pre_image
            .as_ref()
            .ok_or_else(|| anyhow!("attempted to run the executor with no pre_image"))?
            .pc;
        self.monitor.clear_session()?;

        let journal = Journal::default();
        self.env
            .posix_io
            .borrow_mut()
            .with_write_fd(fileno::JOURNAL, journal.clone());

        let mut run_loop = || -> Result<(ExitCode, Segment, MemoryImage)> {
            loop {
                if let Some(exit_code) = self.step()? {
                    let total_cycles = self.total_cycles();
                    tracing::debug!("exit_code: {exit_code:?}, total_cycles: {total_cycles}");
                    assert!(total_cycles <= self.segment_limit);
                    let pre_image = self.pre_image.take().ok_or_else(|| {
                        anyhow!("attempted to run the executor with no pre_image")
                    })?;
                    let post_image = self.monitor.build_image(self.pc)?;
                    let syscalls = mem::take(&mut self.syscalls);
                    let faults = mem::take(&mut self.monitor.faults);
                    let po2 = log2_ceil(total_cycles.next_power_of_two()).try_into()?;
                    let cycles = self.body_cycles.try_into()?;
                    let segment = Segment::new(
                        pre_image,
                        SystemState::from(&post_image),
                        // NOTE: On the last segment, the output is added outside this loop.
                        None,
                        faults,
                        syscalls,
                        exit_code,
                        self.split_insn,
                        po2,
                        self.segments
                            .len()
                            .try_into()
                            .context("Too many segments to fit in u32")?,
                        cycles,
                    );
                    match exit_code {
                        ExitCode::SystemSplit => {
                            let segment_ref = callback(segment)?;
                            self.segments.push(segment_ref);
                            self.split(Some(post_image.into()))?
                        }
                        ExitCode::SessionLimit => {
                            let segment_ref = callback(segment)?;
                            self.segments.push(segment_ref);
                            bail!("Session limit exceeded")
                        }
                        ExitCode::Paused(inner) => {
                            tracing::debug!("Paused({inner}): {}", self.segment_cycle);
                            // Set the pre_image so that the Executor can be run again to resume.
                            // Move the pc forward by WORD_SIZE because halt does not.
                            let mut resume_pre_image = post_image.clone();
                            resume_pre_image.pc += WORD_SIZE as u32;
                            self.split(Some(resume_pre_image.into()))?;
                            return Ok((exit_code, segment, post_image));
                        }
                        ExitCode::Halted(inner) => {
                            tracing::debug!("Halted({inner}): {}", self.segment_cycle);
                            return Ok((exit_code, segment, post_image));
                        }
                        ExitCode::Fault => {
                            tracing::debug!("Fault: {}", self.segment_cycle);
                            return Ok((exit_code, segment, post_image));
                        }
                    };
                };
            }
        };

        let (exit_code, mut final_segment, post_image) = run_loop()?;
        let elapsed = start_time.elapsed();

        // Take (clear out) the list of accessed assumptions.
        // Leave the assumptions cache so it can be used if execution is resumed from pause.
        let assumptions = mem::take(&mut self.env.assumptions.borrow_mut().accessed);

        // Set the session_journal to the committed data iff the the guest set a non-zero output.
        let session_journal = self
            .output_digest
            .and_then(|output_digest| (output_digest != Digest::ZERO).then(|| journal.buf.take()));
        if !exit_code.expects_output() && session_journal.is_some() {
            tracing::debug!(
                "dropping non-empty journal due to exit code {:?}: 0x{}",
                exit_code,
                hex::encode(journal.buf.borrow().as_slice())
            );
        };
        self.exit_code = Some(exit_code);

        // Construct the Output struct for the final segment.
        final_segment.output = exit_code
            .expects_output()
            .then(|| -> Option<Result<_>> {
                session_journal.as_ref().map(|journal| {
                    Ok(Output {
                        journal: journal.clone().into(),
                        assumptions: Assumptions(
                            assumptions
                                .iter()
                                .map(|a| {
                                    Ok(match a {
                                        Assumption::Proven(r) => r.get_claim()?.into(),
                                        Assumption::Unresolved(r) => r.clone(),
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?,
                        )
                        .into(),
                    })
                })
            })
            .flatten()
            .transpose()?;

        // Now that the Output has been populated, call the segment ref callback.
        let segment_ref = callback(final_segment)?;
        self.segments.push(segment_ref);

        tracing::info!("total_cycles = {}", self.total_cycles());
        tracing::info!("segment_count = {}", self.segments.len());
        tracing::info!("execution_time = {:?}", elapsed);

        if let Some(profiler) = self.profiler.take() {
            let report = profiler.borrow_mut().finalize_to_vec();
            std::fs::write(self.env.pprof_out.as_ref().unwrap(), report)?;
        }

        Ok(Session::new(
            mem::take(&mut self.segments),
            session_journal,
            exit_code,
            post_image,
            assumptions,
        ))
    }

    fn split(&mut self, pre_image: Option<Box<MemoryImage>>) -> Result<()> {
        self.pre_image = pre_image;
        self.body_cycles = 0;
        self.split_insn = None;
        self.insn_counter = 0;
        self.segment_cycle = self.init_cycles;
        self.monitor.clear_segment()
    }

    /// Execute a single instruction.
    ///
    /// This can be directly used by debuggers.
    pub fn step(&mut self) -> Result<Option<ExitCode>> {
        if let Some(limit) = self.env.session_limit {
            if self.session_cycle() >= (limit as usize) {
                return Ok(Some(ExitCode::SessionLimit));
            }
        }

        let insn = self.monitor.load_u32(self.pc)?;
        let opcode = OpCode::decode(insn, self.pc)?;

        let frame = if let Some(obj_ctx) = &self.obj_ctx {
            let frames = match obj_ctx.find_frames(self.pc as u64) {
                LookupResult::Output(result) => result.unwrap(),
                _ => unimplemented!(),
            };

            fn decode_frame(frame: Frame<EndianRcSlice<RunTimeEndian>>) -> Option<String> {
                Some(frame.function.as_ref()?.demangle().ok()?.to_string())
            }

            let names: Vec<String> = frames
                .filter_map(|frame| Ok(decode_frame(frame)))
                .collect()
                .unwrap();
            names.first().cloned()
        } else {
            None
        };

        if let Some(frame) = frame {
            tracing::trace!(
                "[{}] pc: 0x{:08x}, insn: 0x{:08x} => {:?}, {frame}",
                self.segment_cycle,
                self.pc,
                opcode.insn,
                opcode
            );
        } else {
            tracing::trace!(
                "[{}] pc: 0x{:08x}, insn: 0x{:08x} => {:?}",
                self.segment_cycle,
                self.pc,
                opcode.insn,
                opcode
            );
        }

        let op_result = if opcode.major == MajorType::ECall {
            self.ecall()?
        } else {
            let registers = self.monitor.load_registers();
            let mut hart = HartState {
                registers,
                pc: self.pc,
                last_register_write: None,
            };

            let mut inst_exec = InstructionExecutor {
                mem: &mut self.monitor,
                hart_state: &mut hart,
            };
            if let Err(err) = inst_exec.step() {
                self.split_insn = Some(self.insn_counter);
                tracing::debug!(
                    "fault: [{}] pc: 0x{:08x} ({:?})",
                    self.segment_cycle,
                    self.pc,
                    err
                );
                self.monitor.undo()?;
                if cfg!(feature = "fault-proof") {
                    return Ok(Some(ExitCode::Fault));
                } else {
                    bail!("rrs instruction executor failed with {:?}", err);
                }
            }

            if let Some(idx) = hart.last_register_write {
                self.monitor.store_register(idx, hart.registers[idx]);
            }

            OpCodeResult::new(hart.pc, None, 0)
        };

        // try to execute the next instruction
        // if the segment limit is exceeded:
        // * don't increment the PC
        // * don't record any activity
        // * return ExitCode::SystemSplit
        // otherwise, commit memory and hart
        let total_pending_cycles = self.total_cycles() + opcode.cycles + op_result.extra_cycles;
        // tracing::debug!(
        //     "cycle: {}, segment: {}, total: {}",
        //     self.segment_cycle,
        //     total_pending_cycles,
        //     self.total_cycles()
        // );

        let exit_code = if total_pending_cycles > self.segment_limit {
            if self.insn_counter == 0 {
                // splitting on the first instruction of the segment means that
                // it's too large to fit into a single cycle.
                bail!("execution of instruction at pc [0x{:08x}] resulted in a cycle count too large to fit into a single segment.", self.pc);
            }
            self.split_insn = Some(self.insn_counter);
            tracing::debug!("split: [{}] pc: 0x{:08x}", self.segment_cycle, self.pc,);
            self.monitor.undo()?;
            Some(ExitCode::SystemSplit)
        } else {
            self.advance(opcode, op_result)
        };
        Ok(exit_code)
    }

    fn advance(&mut self, opcode: OpCode, op_result: OpCodeResult) -> Option<ExitCode> {
        for trace in self.env.trace.iter() {
            trace
                .borrow_mut()
                .trace_callback(TraceEvent::InstructionStart {
                    cycle: self.session_cycle() as u32,
                    pc: self.pc,
                    insn: opcode.insn,
                })
                .unwrap();

            for event in self.monitor.trace_events.iter() {
                trace.borrow_mut().trace_callback(event.clone()).unwrap();
            }
        }

        self.pc = op_result.pc;
        self.insn_counter += 1;
        self.body_cycles += opcode.cycles + op_result.extra_cycles;
        let page_read_cycles = self.monitor.page_read_cycles;
        // tracing::debug!("page_read_cycles: {page_read_cycles}");
        self.segment_cycle = self.init_cycles + page_read_cycles + self.body_cycles;
        self.monitor.commit(self.session_cycle());
        if let Some(syscall) = self.pending_syscall.take() {
            self.syscalls.push(syscall);
        }
        op_result.exit_code
    }

    fn total_cycles(&self) -> usize {
        self.const_cycles
            + self.monitor.page_read_cycles
            + self.monitor.page_write_cycles
            + self.body_cycles
    }

    fn session_cycle(&self) -> usize {
        self.segments.len() * self.segment_limit + self.segment_cycle
    }

    fn ecall(&mut self) -> Result<OpCodeResult> {
        match self.monitor.load_register(REG_T0) {
            ecall::HALT => self.ecall_halt(),
            ecall::INPUT => self.ecall_input(),
            ecall::SOFTWARE => self.ecall_software(),
            ecall::SHA => self.ecall_sha(),
            ecall::BIGINT => self.ecall_bigint(),
            ecall => bail!("Unknown ecall {ecall:?}"),
        }
    }

    fn ecall_halt(&mut self) -> Result<OpCodeResult> {
        let tot_reg = self.monitor.load_register(REG_A0);
        let output_ptr = self.monitor.load_guest_addr_from_register(REG_A1)?;
        let halt_type = tot_reg & 0xff;
        let user_exit = (tot_reg >> 8) & 0xff;
        let output: [u8; DIGEST_BYTES] = self.monitor.load_array_from_guest_addr(output_ptr)?;
        self.output_digest = Some(output.into());

        match halt_type {
            halt::TERMINATE => Ok(OpCodeResult::new(
                self.pc,
                Some(ExitCode::Halted(user_exit)),
                0,
            )),
            halt::PAUSE => Ok(OpCodeResult::new(
                self.pc,
                Some(ExitCode::Paused(user_exit)),
                0,
            )),
            _ => bail!("Illegal halt type: {halt_type}"),
        }
    }

    fn ecall_input(&mut self) -> Result<OpCodeResult> {
        tracing::debug!("ecall(input)");
        let in_addr = self.monitor.load_guest_addr_from_register(REG_A0)?;
        self.monitor
            .load_array_from_guest_addr::<{ DIGEST_WORDS * WORD_SIZE }>(in_addr)?;
        Ok(OpCodeResult::new(self.pc + WORD_SIZE as u32, None, 0))
    }

    fn ecall_sha(&mut self) -> Result<OpCodeResult> {
        let out_state_ptr = self.monitor.load_guest_addr_from_register(REG_A0)?;
        let in_state_ptr = self.monitor.load_guest_addr_from_register(REG_A1)?;
        let mut block1_ptr = self.monitor.load_guest_addr_from_register(REG_A2)?;
        let mut block2_ptr = self.monitor.load_guest_addr_from_register(REG_A3)?;
        let count = self.monitor.load_register(REG_A4);

        let in_state: [u8; DIGEST_BYTES] = self.monitor.load_array_from_guest_addr(in_state_ptr)?;
        let mut state: [u32; DIGEST_WORDS] = bytemuck::cast_slice(&in_state).try_into().unwrap();
        for word in &mut state {
            *word = word.to_be();
        }

        tracing::debug!("Initial sha state: {state:08x?}");
        for _ in 0..count {
            let mut block = [0u32; BLOCK_WORDS];
            for (i, word) in block.iter_mut().enumerate() {
                *word = self
                    .monitor
                    .load_u32_from_guest_addr(block1_ptr + (i * WORD_SIZE) as u32)?;
            }
            for i in 0..DIGEST_WORDS {
                block[DIGEST_WORDS + i] = self
                    .monitor
                    .load_u32_from_guest_addr(block2_ptr + (i * WORD_SIZE) as u32)?;
            }
            tracing::debug!("Compressing block {block:02x?}");
            sha2::compress256(
                &mut state,
                &[*GenericArray::from_slice(bytemuck::cast_slice(&block))],
            );

            block1_ptr += BLOCK_BYTES as u32;
            block2_ptr += BLOCK_BYTES as u32;
        }
        tracing::debug!("Final sha state: {state:08x?}");

        for word in &mut state {
            *word = u32::from_be(*word);
        }

        self.monitor
            .store_region_to_guest_memory(out_state_ptr, bytemuck::cast_slice(&state))?;

        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            SHA_CYCLES * count as usize,
        ))
    }

    // Computes the state transitions for the BIGINT ecall.
    // Take reads inputs x, y, and N and writes output z = x * y mod N.
    // Note that op is currently ignored but must be set to 0.
    fn ecall_bigint(&mut self) -> Result<OpCodeResult> {
        let z_ptr = self.monitor.load_guest_addr_from_register(REG_A0)?;
        let op = self.monitor.load_register(REG_A1);
        let x_ptr = self.monitor.load_guest_addr_from_register(REG_A2)?;
        let y_ptr = self.monitor.load_guest_addr_from_register(REG_A3)?;
        let n_ptr = self.monitor.load_guest_addr_from_register(REG_A4)?;

        let mut load_bigint_le_bytes = |ptr: u32| -> Result<[u8; bigint::WIDTH_BYTES]> {
            let mut arr = [0u32; bigint::WIDTH_WORDS];
            for (i, word) in arr.iter_mut().enumerate() {
                *word = self
                    .monitor
                    .load_u32_from_guest_addr(ptr + (i * WORD_SIZE) as u32)?
                    .to_le();
            }
            Ok(bytemuck::cast(arr))
        };

        if op != 0 {
            anyhow::bail!("ecall_bigint preflight: op must be set to 0");
        }

        // Load inputs.
        let x = U256::from_le_bytes(load_bigint_le_bytes(x_ptr)?);
        let y = U256::from_le_bytes(load_bigint_le_bytes(y_ptr)?);
        let n = U256::from_le_bytes(load_bigint_le_bytes(n_ptr)?);

        // Compute modular multiplication, or simply multiplication if n == 0.
        let z: U256 = if n == U256::ZERO {
            x.checked_mul(&y).unwrap()
        } else {
            let (w_lo, w_hi) = x.mul_wide(&y);
            let w = w_hi.concat(&w_lo);
            let z = w.rem(&NonZero::<U512>::from_uint(n.resize()));
            z.resize()
        };

        // Store result.
        for (i, word) in bytemuck::cast::<_, [u32; bigint::WIDTH_WORDS]>(z.to_le_bytes())
            .into_iter()
            .enumerate()
        {
            self.monitor
                .store_u32_to_guest_memory(z_ptr + (i * WORD_SIZE) as u32, word.to_le())?;
        }

        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            BIGINT_CYCLES,
        ))
    }

    fn ecall_software(&mut self) -> Result<OpCodeResult> {
        let to_guest_ptr = self.monitor.load_register(REG_A0);
        if !is_guest_memory(to_guest_ptr) && to_guest_ptr != 0 {
            bail!("address 0x{to_guest_ptr:08x} is an invalid guest address");
        }
        let to_guest_words = self.monitor.load_register(REG_A1);
        let name_ptr = self.monitor.load_guest_addr_from_register(REG_A2)?;
        let syscall_name = self.monitor.load_string_from_guest_memory(name_ptr)?;
        tracing::trace!(
            "Guest called syscall {syscall_name:?} requesting {to_guest_words} words back"
        );

        let chunks = align_up(to_guest_words as usize, WORD_SIZE);

        let syscall = if let Some(syscall) = self.pending_syscall.clone() {
            tracing::debug!("Replay syscall: {syscall:?}");
            syscall
        } else {
            let mut to_guest = vec![0; to_guest_words as usize];
            let handler = self
                .syscall_table
                .get_syscall(&syscall_name)
                .ok_or(anyhow!("Unknown syscall: {syscall_name:?}"))?;
            let (a0, a1) =
                handler
                    .borrow_mut()
                    .syscall(&syscall_name, &mut self.monitor, &mut to_guest)?;
            let syscall = SyscallRecord {
                to_guest,
                regs: (a0, a1),
            };
            self.pending_syscall = Some(syscall.clone());
            syscall
        };

        let (a0, a1) = syscall.regs;
        if to_guest_ptr != 0 {
            // the guest pointer is set to null for cases where the guest is
            // sending info to the host so there's no data to write to guest
            // memory.
            self.monitor.store_region_to_guest_memory(
                to_guest_ptr,
                bytemuck::cast_slice(&syscall.to_guest),
            )?;
        }
        self.monitor.store_register(REG_A0, a0);
        self.monitor.store_register(REG_A1, a1);

        tracing::trace!("Syscall returned a0: {a0:#X}, a1: {a1:#X}, chunks: {chunks}");

        // One cycle for the ecall cycle, then one for each chunk or
        // portion thereof then one to save output (a0, a1)
        Ok(OpCodeResult::new(
            self.pc + WORD_SIZE as u32,
            None,
            1 + chunks + 1,
        ))
    }
}
