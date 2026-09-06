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
    axhal::console::emergency_diagnostic_print(format_args!("{}\n", info));
    axhal::console::emergency_diagnostic_print(format_args!(
        "{}\n",
        axbacktrace::Backtrace::capture()
    ));
    axhal::power::system_off()
}
