// Copyright 2025 The Axvisor Team
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

use core::panic::PanicInfo;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Crash-kexec must run before logging: a panic may have interrupted the
    // console lock holder, in which case even one recursive print would spin
    // forever and make the preloaded recovery kernel unreachable.
    crate::invoke_panic_crash_hook();
    // The screen is the only output a machine without a serial port has, so it
    // is painted before the diagnostic transport is asked to do anything.  The
    // backtrace is captured once and shared with the UART line below, so this
    // adds no second unwind to a path which may already own nothing it can
    // rely on.  A screen which cannot be painted returns without effect and the
    // rest of this handler runs exactly as it did without it.
    let backtrace = axbacktrace::Backtrace::capture();
    crate::invoke_panic_screen_hook(info, &format_args!("{backtrace}"));
    axhal::console::emergency_diagnostic_print(format_args!("{}\n", info));
    axhal::console::emergency_diagnostic_print(format_args!("{backtrace}\n"));
    axhal::power::system_off()
}
