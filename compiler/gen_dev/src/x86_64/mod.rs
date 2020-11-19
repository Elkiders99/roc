use crate::{Backend, Env, Relocation};
use bumpalo::collections::Vec;
use roc_collections::all::{ImSet, MutMap};
use roc_module::symbol::Symbol;
use roc_mono::ir::{Literal, Stmt};
use roc_mono::layout::Layout;
use target_lexicon::{CallingConvention, Triple};

mod asm;
use asm::Register;

const RETURN_REG: Register = Register::RAX;

#[derive(Clone, Debug, PartialEq)]
enum SymbolStorage<'a> {
    Literal(Literal<'a>),
    Register(Register, Layout<'a>),
    Stack(u32, Layout<'a>),
}

pub struct X86_64Backend<'a> {
    env: &'a Env<'a>,
    buf: Vec<'a, u8>,

    /// leaf_proc is true if the only calls this function makes are tail calls.
    /// If that is the case, we can skip emitting the frame pointer and updating the stack.
    leaf_proc: bool,

    last_seen_map: MutMap<Symbol, *const Stmt<'a>>,
    // This will need to hold info a symbol is held in a register or on the stack as well.
    symbols_map: MutMap<Symbol, SymbolStorage<'a>>,
    // This is gonna need to include a lot of data. Right now I can think of quite a few.
    // Registers order by priority with info of what data is stored in them.
    // Scope with knows were all variables are currently stored.X86_64Backend

    // Since this is x86_64 the calling convetions is really just windows or linux/macos.
    // Hopefully this will be easy to extract into a trait somehow. Cause probably don't want if's everywhere.
    // Also, don't really want to build an x86_64-win backend specifically for it.

    // function parameter registers listed by order. Need to know the float equivalent registers as well.
    // Probably need to encode stack parameter knowledge here too.
    // return parameter register. This includes dealing with multiple value returns.
    gp_param_regs: &'static [Register],

    // A linear scan of an array may be faster than a set technically.
    // That being said, fastest would likely be a trait based on calling convention/register.
    caller_saved_regs: ImSet<Register>,
    callee_saved_regs: ImSet<Register>,
    shadow_space_size: u8,
    red_zone_size: u8,

    // not sure how big this should be u16 is 64k. I hope no function uses that much stack.
    stack_size: u16,
}

impl<'a> Backend<'a> for X86_64Backend<'a> {
    fn new(env: &'a Env, target: &Triple) -> Result<Self, String> {
        match target.default_calling_convention() {
            Ok(CallingConvention::SystemV) => Ok(X86_64Backend {
                env,
                leaf_proc: true,
                buf: bumpalo::vec!(in env.arena),
                last_seen_map: MutMap::default(),
                symbols_map: MutMap::default(),
                gp_param_regs: &[
                    Register::RDI,
                    Register::RSI,
                    Register::RDX,
                    Register::RCX,
                    Register::R8,
                    Register::R9,
                ],
                // TODO: stop using vec! here. I was just have trouble with some errors, but it shouldn't be needed.
                caller_saved_regs: ImSet::from(vec![
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::RSP,
                    Register::RSI,
                    Register::RDI,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ]),
                callee_saved_regs: ImSet::from(vec![
                    Register::RBX,
                    Register::RBP,
                    Register::R12,
                    Register::R13,
                    Register::R14,
                    Register::R15,
                ]),
                shadow_space_size: 0,
                red_zone_size: 128,
                stack_size: 0,
            }),
            Ok(CallingConvention::WindowsFastcall) => Ok(X86_64Backend {
                env,
                leaf_proc: true,
                buf: bumpalo::vec!(in env.arena),
                last_seen_map: MutMap::default(),
                symbols_map: MutMap::default(),
                gp_param_regs: &[Register::RCX, Register::RDX, Register::R8, Register::R9],
                caller_saved_regs: ImSet::from(vec![
                    Register::RAX,
                    Register::RCX,
                    Register::RDX,
                    Register::R8,
                    Register::R9,
                    Register::R10,
                    Register::R11,
                ]),
                callee_saved_regs: ImSet::from(vec![
                    Register::RBX,
                    Register::RBP,
                    Register::RSI,
                    Register::RSP,
                    Register::RDI,
                    Register::R12,
                    Register::R13,
                    Register::R14,
                    Register::R15,
                ]),
                shadow_space_size: 32,
                red_zone_size: 0,
                stack_size: 0,
            }),
            x => Err(format!("unsupported backend: {:?}", x)),
        }
    }

    fn env(&self) -> &'a Env<'a> {
        self.env
    }

    fn reset(&mut self) {
        self.symbols_map.clear();
        self.buf.clear();
    }

    fn last_seen_map(&mut self) -> &mut MutMap<Symbol, *const Stmt<'a>> {
        &mut self.last_seen_map
    }

    fn set_symbol_to_lit(&mut self, sym: &Symbol, lit: &Literal<'a>) {
        self.symbols_map
            .insert(*sym, SymbolStorage::Literal(lit.clone()));
    }

    fn free_symbol(&mut self, sym: &Symbol) {
        self.symbols_map.remove(sym);
    }

    fn return_symbol(&mut self, sym: &Symbol) -> Result<(), String> {
        self.load_symbol(RETURN_REG, sym)
    }

    fn finalize(&mut self) -> Result<(&'a [u8], &[Relocation]), String> {
        // TODO: handle allocating and cleaning up data on the stack.
        let mut out = bumpalo::vec![in self.env.arena];
        if self.requires_stack_modification() {
            asm::push_register64bit(&mut out, Register::RBP);
            asm::mov_register64bit_register64bit(&mut out, Register::RBP, Register::RSP);
        }
        out.extend(&self.buf);

        if self.requires_stack_modification() {
            asm::pop_register64bit(&mut out, Register::RBP);
        }
        asm::ret_near(&mut out);

        Ok((out.into_bump_slice(), &[]))
    }
}

/// This impl block is for ir related instructions that need backend specific information.
/// For example, loading a symbol for doing a computation.
impl<'a> X86_64Backend<'a> {
    fn requires_stack_modification(&self) -> bool {
        !self.leaf_proc
            || self.stack_size < self.shadow_space_size as u16 + self.red_zone_size as u16
    }

    fn load_symbol(&mut self, dst: Register, sym: &Symbol) -> Result<(), String> {
        let val = self.symbols_map.get(sym);
        match val {
            Some(SymbolStorage::Literal(Literal::Int(x))) => {
                let val = *x;
                if val <= i32::MAX as i64 && val >= i32::MIN as i64 {
                    asm::mov_register64bit_immediate32bit(&mut self.buf, dst, val as i32);
                } else {
                    asm::mov_register64bit_immediate64bit(&mut self.buf, dst, val);
                }
                Ok(())
            }
            Some(x) => Err(format!("symbol, {:?}, is not yet implemented", x)),
            None => Err(format!("Unknown return symbol: {}", sym)),
        }
    }
}
