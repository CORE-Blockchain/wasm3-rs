use alloc::boxed::Box;
use core::ptr::{self, NonNull};
use core::slice;

use crate::environment::Environment;
use crate::error::{Error, Result};
use crate::function::{Function, NNM3Function, RawCall};
use crate::runtime::Runtime;
use crate::utils::{cstr_to_str, eq_cstr_str, rt_check};
use crate::wasm3_priv;

/// A parsed module which can be loaded into a [`Runtime`].
pub struct ParsedModule {
    raw: ffi::IM3Module,
    env: Environment,
}

impl ParsedModule {
    /// Parses a wasm module from raw bytes.
    pub fn parse(env: &Environment, bytes: &[u8]) -> Result<Self> {
        assert!(bytes.len() <= !0u32 as usize);
        let mut module = ptr::null_mut();
        let res = unsafe {
            ffi::m3_ParseModule(
                env.as_ptr(),
                &mut module,
                bytes.as_ptr(),
                bytes.len() as u32,
            )
        };
        Error::from_ffi_res(res).map(|_| ParsedModule {
            raw: module,
            env: env.clone(),
        })
    }

    pub(crate) fn as_ptr(&self) -> ffi::IM3Module {
        self.raw
    }

    /// The environment this module was parsed in.
    pub fn environment(&self) -> &Environment {
        &self.env
    }
}

impl Drop for ParsedModule {
    fn drop(&mut self) {
        unsafe { ffi::m3_FreeModule(self.raw) };
    }
}

/// A loaded module belonging to a specific runtime. Allows for linking and looking up functions.
/// This is just a token which can be used to perform the desired actions on the runtime it belongs to.
// needs no drop as loaded modules will be cleaned up by the runtime
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Module {
    raw: ffi::IM3Module,
    raw_rt: ffi::IM3Runtime,
}

impl Module {
    /// Parses a wasm module from raw bytes.
    #[inline]
    pub fn parse(environment: &Environment, bytes: &[u8]) -> Result<ParsedModule> {
        ParsedModule::parse(environment, bytes)
    }

    /// Links the given function to the corresponding module and function name.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    ///
    /// * a memory allocation failed
    /// * no function by the given name in the given module could be found
    /// * the function has been found but the signature did not match
    pub fn link_function<ARGS, RET>(
        self,
        rt: &mut Runtime,
        module_name: &str,
        function_name: &str,
        f: RawCall,
    ) -> Result<()>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
    {
        rt_check(rt, self.raw_rt);
        let func = self.find_import_function(module_name, function_name)?;
        Function::<ARGS, RET>::validate_sig(func)
            .and_then(|_| unsafe { self.link_func_impl(rt, func, f) })
    }

    /// Links the given closure to the corresponding module and function name.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    ///
    /// * a memory allocation failed
    /// * no function by the given name in the given module could be found
    /// * the function has been found but the signature did not match
    pub fn link_closure<ARGS, RET, F>(
        self,
        rt: &mut Runtime,
        module_name: &str,
        function_name: &str,
        closure: F,
    ) -> Result<()>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
        F: FnMut(ARGS) -> RET + 'static,
    {
        rt_check(rt, self.raw_rt);
        let func = self.find_import_function(module_name, function_name)?;
        Function::<ARGS, RET>::validate_sig(func)?;
        let mut closure = Box::pin(closure);
        unsafe { self.link_closure_impl(rt, func, closure.as_mut().get_unchecked_mut()) }?;
        rt.push_closure(closure);
        Ok(())
    }

    /// Looks up a function by the given name in this module.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    ///
    /// * a memory allocation failed
    /// * no function by the given name in the given module could be found
    /// * the function has been found but the signature did not match
    pub fn find_function<ARGS, RET>(
        self,
        rt: &Runtime,
        function_name: &str,
    ) -> Result<Function<ARGS, RET>>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
    {
        rt_check(rt, self.raw_rt);
        let func = unsafe {
            slice::from_raw_parts_mut(
                if (*self.raw).functions.is_null() {
                    NonNull::dangling().as_ptr()
                } else {
                    (*self.raw).functions
                },
                (*self.raw).numFunctions as usize,
            )
            .iter_mut()
            .find(|func| eq_cstr_str(func.name, function_name))
            .map(NonNull::from)
            .ok_or(Error::FunctionNotFound)?
        };
        Function::from_raw(self.raw_rt, func).and_then(Function::compile)
    }

    /// Looks up a function by its index in this module.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    ///
    /// * a memory allocation failed
    /// * the index is out of bounds
    /// * the function has been found but the signature did not match
    pub fn function<ARGS, RET>(
        self,
        rt: &Runtime,
        function_index: usize,
    ) -> Result<Function<ARGS, RET>>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
    {
        rt_check(rt, self.raw_rt);
        let func = unsafe {
            slice::from_raw_parts_mut(
                if (*self.raw).functions.is_null() {
                    NonNull::dangling().as_ptr()
                } else {
                    (*self.raw).functions
                },
                (*self.raw).numFunctions as usize,
            )
            .get(function_index)
            .map(NonNull::from)
            .ok_or(Error::FunctionNotFound)?
        };
        Function::from_raw(self.raw_rt, func).and_then(Function::compile)
    }

    /// The name of this module.
    pub fn name(self, rt: &Runtime) -> &str {
        rt_check(rt, self.raw_rt);
        unsafe { cstr_to_str((*self.raw).name) }
    }

    /// Links wasi to this module.
    #[cfg(feature = "wasi")]
    pub fn link_wasi(self, rt: &mut Runtime) -> Result<()> {
        rt_check(rt, self.raw_rt);
        unsafe { Error::from_ffi_res(ffi::m3_LinkWASI(self.raw)) }
    }

    /// Links libc to this module.
    pub fn link_libc(self, rt: &mut Runtime) -> Result<()> {
        rt_check(rt, self.raw_rt);
        unsafe { Error::from_ffi_res(ffi::m3_LinkLibC(self.raw)) }
    }
}

impl Module {
    pub(crate) fn from_raw(raw_rt: ffi::IM3Runtime, raw: ffi::IM3Module) -> Self {
        Module { raw, raw_rt }
    }

    unsafe fn link_func_impl(
        self,
        rt: &mut Runtime,
        mut m3_func: NNM3Function,
        func: RawCall,
    ) -> Result<()> {
        let page = wasm3_priv::AcquireCodePageWithCapacity(rt.as_ptr(), 2);
        if page.is_null() {
            Error::from_ffi_res(ffi::m3Err_mallocFailedCodePage)
        } else {
            m3_func.as_mut().compiled = wasm3_priv::GetPagePC(page);
            m3_func.as_mut().module = self.raw;
            wasm3_priv::EmitWord_impl(page, crate::wasm3_priv::op_CallRawFunction as _);
            wasm3_priv::EmitWord_impl(page, func as _);

            wasm3_priv::ReleaseCodePage(rt.as_ptr(), page);
            Ok(())
        }
    }

    unsafe fn link_closure_impl<ARGS, RET, F>(
        self,
        rt: &mut Runtime,
        mut m3_func: NNM3Function,
        closure: *mut F,
    ) -> Result<()>
    where
        ARGS: crate::WasmArgs,
        RET: crate::WasmType,
        F: FnMut(ARGS) -> RET + 'static,
    {
        unsafe extern "C" fn _impl<ARGS, RET, F>(
            runtime: ffi::IM3Runtime,
            sp: *mut u64,
            _mem: *mut cty::c_void,
            closure: *mut cty::c_void,
        ) -> *const cty::c_void
        where
            ARGS: crate::WasmArgs,
            RET: crate::WasmType,
            F: FnMut(ARGS) -> RET + 'static,
        {
            // use https://doc.rust-lang.org/std/primitive.pointer.html#method.offset_from once stable
            let stack_base = (*runtime).stack as ffi::m3stack_t;
            let stack_occupied = (sp as usize - stack_base as usize) / core::mem::size_of::<u64>();
            let stack =
                slice::from_raw_parts_mut(sp, (*runtime).numStackSlots as usize - stack_occupied);

            let args = ARGS::retrieve_from_stack(stack);
            let ret = (&mut *closure.cast::<F>())(args);
            ret.put_on_stack(stack);
            ffi::m3Err_none as _
        }

        let page = wasm3_priv::AcquireCodePageWithCapacity(rt.as_ptr(), 3);
        if page.is_null() {
            Error::from_ffi_res(ffi::m3Err_mallocFailedCodePage)
        } else {
            m3_func.as_mut().compiled = wasm3_priv::GetPagePC(page);
            m3_func.as_mut().module = self.raw;
            wasm3_priv::EmitWord_impl(page, crate::wasm3_priv::op_CallRawFunctionEx as _);
            wasm3_priv::EmitWord_impl(page, _impl::<ARGS, RET, F> as _);
            wasm3_priv::EmitWord_impl(page, closure.cast());

            wasm3_priv::ReleaseCodePage(rt.as_ptr(), page);
            Ok(())
        }
    }

    fn find_import_function(self, module_name: &str, function_name: &str) -> Result<NNM3Function> {
        unsafe {
            slice::from_raw_parts_mut(
                if (*self.raw).functions.is_null() {
                    NonNull::dangling().as_ptr()
                } else {
                    (*self.raw).functions
                },
                (*self.raw).numFunctions as usize,
            )
            .iter_mut()
            .filter(|func| eq_cstr_str(func.import.moduleUtf8, module_name))
            .find(|func| eq_cstr_str(func.import.fieldUtf8, function_name))
            .map(NonNull::from)
            .ok_or(Error::FunctionNotFound)
        }
    }
}

#[test]
fn module_parse() {
    let env = Environment::new().expect("env alloc failure");
    let fib32 = [
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x06, 0x01, 0x60, 0x01, 0x7f, 0x01,
        0x7f, 0x03, 0x02, 0x01, 0x00, 0x07, 0x07, 0x01, 0x03, 0x66, 0x69, 0x62, 0x00, 0x00, 0x0a,
        0x1f, 0x01, 0x1d, 0x00, 0x20, 0x00, 0x41, 0x02, 0x49, 0x04, 0x40, 0x20, 0x00, 0x0f, 0x0b,
        0x20, 0x00, 0x41, 0x02, 0x6b, 0x10, 0x00, 0x20, 0x00, 0x41, 0x01, 0x6b, 0x10, 0x00, 0x6a,
        0x0f, 0x0b,
    ];
    let _ = Module::parse(&env, &fib32[..]).unwrap();
}
