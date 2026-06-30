# History Rewrite Notice

This repository rewrote the public `main` history on 2026-06-30 for OSComp submission hygiene.

The rewrite had three goals:

1. Backfill the latest `README.md`, `LICENSE`, `NOTICE`, `docs/oscomp2026_report.pdf`, and `docs/oscomp2026_slides.pdf` into every existing commit, including the initial commit.
2. Remove the misspelled `docs/oscomp2026_reprot.pdf` path from the rewritten `main` history.
3. Normalize AI-assisted commit trailers to `Co-Authored-By: Codex <noreply@openai.com>`.

No source-level behavior change was intended by the rewrite itself. Commit object IDs changed because commit trees and commit messages changed.

## Tip Mapping

- Pre-rewrite public `main` tip: `9ebbbf0ed283c72972ed4b35f35068c8ae657c45`
- After document backfill tip: `b4869c8fa6938d600e6c0819388150ba5a9fe839`
- After coauthor normalization tip for the preexisting 335 commits: `369f7606280d111d73f7c198389e9a341210c5c2`
- This notice and the README update are appended after that mapped history and have no predecessor in the old history.

## Commit Mapping

The rows below map commits by chronological order and verified matching subject lines.

| Pre-rewrite hash | After document backfill | Current normalized hash | Subject |
|---|---|---|---|
| `982ba68a98f3adda3403a2f5485be54ac2edf0b9` | `d125e7079e96353de1e9b65776272152870ab966` | `1154947bacd2a25ea7f3739f1694b9cc0a7cdebf` | chore(repo): initial commit |
| `c8b03154e850bc4df7aa3d710be3816311045c29` | `000f0e2d3450218193455dcb140d864cb271853c` | `15725c25b47addc39a65cc3e0bc1dc10bc0900aa` | fix(procfs): avoid recursive task directories |
| `0321c7511936a62ee993e597e4f885a99b483b49` | `ea21080040c35e9226839174b465f935514813e2` | `9d4a3351c25dc92f0b03969b1f375d3f476bf47d` | build(toolchain): patch nightly-2025-05-20 compatibility |
| `feec4609c30e86da6f08bc5e7938c98b3ac1c54a` | `7dbef633d46969fbba43e32e451e28a0879a4366` | `aa98717bf6232df2ad584475d962e442f3ad2427` | build(toolchain): trim vendored compatibility patches |
| `41eca613bf300231ee1fa9bbb12d138484bd2093` | `5fbea3101d8ab958fd9f450467a5e2f5c909d492` | `d28099bdae0aabf54d92597d6ca03438a80ee0ff` | build(toolchain): align main toolchain with contest nightly |
| `b5bebc87f6a4e2acbf55fae4a2e66afa82cef362` | `8cf7feaa7bb159519cc4c17f162c64c00b83a1ea` | `a8f2b926e050692c6688b581294e3e848079fc97` | style(repo): format pending changes |
| `d1e92299cfaf9314918055ba90495460890138e3` | `8b9a0662c4f9220179acd7cbb88044e5487306bc` | `f6fe39f8f46076fffd807dc59e8c15ef363340f7` | fix(net): return peer address instead of local address in accept4 |
| `9a81d051775ed183af02c4b3025b7da8bb8bdc01` | `6af27c060a112bfc9a9ab9de5ca33189991d21c4` | `235b12716d809b8c69213d3c0778997bd6fa4062` | fix(sched): write priority in sched_getparam instead of returning empty |
| `e7a60ceca245f5ae4d681125e12948ba6e26f7a1` | `26b292d8e696a99fa3f52c3991786471c80e020f` | `25f9cc38e1ed8ce6ffbdb04cfbd401205345c849` | fix(sched): support setting affinity for arbitrary tasks by pid |
| `c9868fffad3c475b32f97189377e8aeff4b2aa06` | `d326f7dfebb6a6420cdc4897c9cce0f897914a69` | `92be36f2aadce2d3b35412c6b1fb0f99c55f4c22` | feat(sched): implement sched_getscheduler/setscheduler/getparam properly |
| `0bd44ca2118b30853d57e6e51efc0f4c6f1e2ad8` | `152777772e394f8194971c20d57fec01bf51a994` | `ca5692834456c787b515f3d1bc85a854e9231253` | feat(net): store SO_SNDBUF/SO_RCVBUF values instead of silently ignoring |
| `eb89ec36e3e6d7380567d17649a898edb61a0f01` | `f30931720aa6ff5d03dd98fac8f3210928297dab` | `ae3aef5f138eb78546ecf3be478bc30ea4469288` | fix(sched): add sched_attr support |
| `1cb1b69736a612030eab4429e468544e2897785b` | `842a12e73106c211a590655bfe86bf9cd700bec1` | `ee3ad1f72768f0b4d9eab65631209f557357e8a3` | feat(sched): switch scheduler from RR to CFS |
| `fe32e7342ef6dc4fd3fe219c522007eaa6c79554` | `9bc0eae4edbe2fe3b92e64269b4f30f457dccd75` | `2f7edb316f7694b4daec93418530b11b795af8b5` | fix(waitpid): prioritize child status over signal interruption |
| `69fc9f4979c6899fb5befae3ea2e70928b8c10eb` | `e1f51abb7e81ae981e5456010db7957f0dd6b85a` | `5330308d4ee5db7bb11b646c6675f48c671e13e1` | feat(waitpid): add peek methods to ProcessData for WNOWAIT |
| `0edb06053868a14e8b8895d4ba3d1a1eb1b7ee6c` | `f1c3ef06f35ca528262d06e60f72575020679c70` | `cc0ec5e8d25b67086a5b7d3232aae78fe8bafc15` | feat(waitpid): implement WALL/WCLONE filtering with exit_signal cache |
| `c2024330941b5801b5e95af9c1daa49ce83ea3d2` | `04e391bf2b2b99c4200af72b986aa54c11dc0418` | `bbd68dd9a8750d805dd6702778921d9dd17c54d2` | fix(waitpid): refresh children list on each poll iteration |
| `1be2679a912c605cd47a7d73ca8b9f52de1a543c` | `05c3244822b89d7a61e4b0b2355aeb20cc3a0161` | `496cbf3b136bc062b30949001a13314a2426feb8` | feat(waitpid): implement WNOWAIT support |
| `8e1e606302927071c0798786d41dedc8eb8685bb` | `116e70340c452254b4abf73f8ce66094384c2151` | `40550dd2b6ab56e82bac83e409db571048085c74` | feat(waitpid): add rusage support to wait4 and fix RUSAGE_CHILDREN |
| `6ccda6378d09abe7c218bc38c03d7aea2397239a` | `8e50581348af0727df610a696eb2c0465d5358b5` | `5da00444fc324f6781837f88098177f9dcc8054d` | feat(waitpid): implement waitid syscall |
| `b72bc60ebf3f8e383b1af5dafb43700fc7baecd0` | `c5e3c9dd6419b2392b06ffe852fbd13f78998085` | `07de702ea042aa04f7511c1b7a4e870be2e6d08b` | feat(sched): add runtime CFS classes and scheduler state |
| `6c409b02bfa837681dfa4cc15ea641fe653d3095` | `06a52a1c9649650c4d7faeaa9be7f35b7af954a6` | `3fe155bbdd3a68f778a76685f2ab3870cc82de90` | feat(wait): add durable child accounting and wait snapshots |
| `fb65a7ad380d4b5ffd465242b5b8b40897e4700b` | `40c2ee3ea0f53ec7181903fdcd9aa77f37c9e5a1` | `43381869ba02821be341709038c680570fb3a885` | fix(sched): harden runtime scheduler state updates |
| `d1c792203973ac839a8cda365fc8192c0509af21` | `36fe2d69cf2216d2fa75ab26d5a692bb4ab422c3` | `088a201e58e1e5899a94daf9cd351f7abd44af28` | feat(signal): add restartable wait syscall state |
| `cd65d41b1b06c71648ac2299577c68999e1cd1af` | `a074f4a94a7b5abc38203bbf833c8e7191352772` | `b6256f13a35ebbf2271fa1ffe8321239b4410dda` | fix(signal): complete syscall restart state handling |
| `5e25aea261496ed2b9d4100464ad4fd8624cccfd` | `3910cf25e0d1d037528adad7bae83596904232a8` | `873d74ca48d15a2df24a91a1da51606c1935d395` | test(signal): cover restart resume paths |
| `51460e15ac7dbba77d5c4949e0ece954ba5b20b1` | `f78dc05b87a7d54def31e503defceb6a4c2549c7` | `53d3c0acc40fdd7cab7df3c0cec7662b7466047a` | fix(signal): centralize syscall restart enrollment |
| `3fb83c51f5647f94d8fb7d46faa9ebcda5320a76` | `33a34960dc0ae0c7bb9175b29300463a8d664e79` | `73a253e1eb551b006ac52b8f358288afed48604e` | fix(signal): preserve masks for poll-family waits |
| `b2615495f6f97f2fba2e5b3b7acad69f1db09091` | `aa0a6e252a4450e05bf777d9d13a40eca2fee618` | `5c861fad78c0d11985a87d13bb635cbd2bb33d96` | fix(signal): tighten futex restart handling |
| `5d884edcd56ee41081d506fe6065529bb8129032` | `5a7e36fde113ecab39b45f3b605589a07ab350e8` | `9cf75a258190b6fc22a1f36c14a8402221da1c29` | feat(signal): add restart_syscall timeout blocks |
| `8adc6a0f19ac781236dd336713b3096b4cb9ce84` | `97b7c3125422444ae8388a920f3ef0cfabef90a0` | `8f8cf21edabba55158983a4361bd4a5bc4ee9007` | merge(main): add restart_syscall timeout accounting |
| `4228eb0b06e25c7d685c1338bee73f0eb7c849cb` | `64902d71eb036bcd17c6db7a9e79a2ec2e487ff1` | `7381fec6cff35d62b67d305394102bf03a64a7b4` | fix(futex): keep timed waiters pending until wake |
| `d09ce7e71325573249c1a308cca10cfcc5a1b5f2` | `6a7bd8ffb9869b960c552bafe2525797a68a522e` | `ba5b2c2c905b16a524d0bf5bf15b050899638488` | merge(main): fix futex timed waiter completion |
| `a9227ae2ed1d8a7037f798e7fca2a1fa61b5c437` | `8ca14d5d8ebebfe76f218c6e5485dcc7db2d04ed` | `5f562315d7d597f477b6b4be4d349849bf47e5ca` | fix(fs): normalize root path semantics |
| `4cdc23fedc46acbd261a184e2e9e9578e7817f9f` | `6f1a638d7c10eceeffa13e2244ec94af4034b9ab` | `5c212807bd73ac2506f10ecc1a2e5e8b2479f2d0` | fix(task): implement shared-vm vfork semantics |
| `6c291a8ed8cc1e7ae2a0a65b6292bfd1df7b2dde` | `c7d492a1df26b2e02f0b48093e99a7e25e42e239` | `7f198eb4f1a2e7d03b667f3ba87b585c846a7ba2` | fix(mm): optimize loongarch user-copy path |
| `513af16b87877df802b0625da51ca0839618c976` | `d94c6a315f42ce45e41e29cd156510b9a5ca8634` | `e5ee2f9a0eff6250907b2b16a006b29ee035c72b` | feat(sched): add RT policy support for scheduler syscalls |
| `dc2253a5718bffc98fe54239fd39bd56c5007860` | `992c62ded149423b9751c2cafdf43c24b4b01de3` | `846bdcf6c6cfe5184b8aa616149b3dcbe5f2e617` | fix(task): prioritize exited-task reclamation |
| `5a37826e7db9221e7b2b959ff3fd92d0830f6a75` | `6cc0205c74fa7943338b9a4c881e939f8f7b6691` | `aba4da628dda30f020b78c42de7a6ab2838596c8` | fix(time): validate clock ids for clock syscalls |
| `a8520fef0635daee5e58ea4ff2a726638c491030` | `7676e948300fb4b17a2c0379fe62c78a6757eaa2` | `24e5768eade1297ac8f64e328015c12c64cce6c3` | fix(timer): program early monotonic wakeups |
| `49e263d7d662734ddbffec279ac0d2eaa4f1a082` | `937b6eaedf91afe871daee96f8546e146bf3fded` | `04afede792f263e9a05c18e6c85c9c7427ecf458` | fix(futex): harden wait timeout and requeue semantics |
| `7ffbccb3da12fcafd8c16a797a6408f896fe5a37` | `d347e63559f179bd312b6c1a12e8fd8a1a33d422` | `d550e3416e203ace61c8edc4e61efb96c9a5c350` | fix(task): reclaim exited thread stacks early |
| `da169af74a88875146f2c2e9205023e57e721e47` | `27c7dfa1cb772be40d9f994f810d061d8d8ba98e` | `1508375bb1f8c89a88e9794f47cddb2a37bdec37` | fix(task): defer stack reuse until final task drop |
| `be7d50dd9d1207776a3642de2884ddc1de0cf1fc` | `c50f990d531ef1dcbffc4bb70ecaea1e3eb263a0` | `69b765208f709c094eee87523a3d778f568a1c20` | perf(task): remove per-layout stack cache cap |
| `59eda8684d439756c35fb81f7af8bffca5d5a0e6` | `8691120e97525579b59912ebcbd46f7d394d5a7a` | `8b3a3664f915cc0aa9b5fa8b6b32292ca7cf8f23` | perf(task): stop prioritizing gc over joiners |
| `8a9a723797bc4fe29ef2c04d246b2c8fe7c29eda` | `1e4cd497931f3153b82eeef20f8f9a9e3c14b479` | `f0cf91bba62051bbe9b287e1cbeb4bdef9e37e57` | feat(mm): scale caches with physical memory |
| `a488e804d9d19db2050a4a2555fdab254ea8a64c` | `d633727e701f10c4f645498842c6a2d7b46737ab` | `cc97508a4a57e7c8e776229a7d327cf818786763` | perf(mm): grow page cache with larger RAM |
| `08740c6600f30a115fcaa52a16a7cd0bd454b77e` | `5acfff25f33aa715d786ae3102f03ba4dc38539f` | `80620f40925defb80cb6a7cc87c42b009d5d7e33` | perf(mm): coalesce anon vmas and reduce fault churn |
| `62d01a8352c71d602769823d39f1d297fd3dc25a` | `39650283d151ecf08cb379e588fdba78a6cc223a` | `e6fe5c4b9e8492a5613ef7055306a137d1f3a2ca` | perf(task): reclaim exiting address spaces early |
| `f7030c316a24b42b92c31ebf7f36199daa6323df` | `4dff509729435b1d375a6e7509d4f57e14628b88` | `aa610a0a022a7769a578b53a2d7faf418b82c6f8` | perf(mm): skip sparse cow leaf scans |
| `2202ab79c7241cb51533566831ceac06df799dbb` | `48b4bd165d75dfa8993693897ea8a17fb786ab38` | `7ec806632623bec4d03df9e4de3c7c8cd410772f` | perf(mm): avoid empty cow unmaps |
| `482e476716b423bf1953df2526982127b28036f4` | `44645cfafa17e4229fd456b03a5bd595aee7d2c0` | `a749c808d88cada5d542de8c91655fab281649bb` | perf(mm): batch cow unmaps through page-table drain |
| `db9cacee8b22d5e912a430985401766976c5810a` | `ffe0c11ec97dba85d90839711bbbce015856b8d7` | `b6c0b4c665edcff4abc8cc2fa994374af5d7d975` | perf(mm): add append-biased kernel area placement |
| `0779ca06ebbf0096d8e78d2450317788fa7d7dcc` | `f260e50c51e2725176d5ff6074793584317e5a6e` | `6fd07f78991d1ca15036c9707d92b3727b7504eb` | fix(task): remove unsafe kernel stack cache |
| `51023bc6d5a048fd0bdca876435757349f612288` | `58c92a3ed9bf90acdce0588827f841aa8190efc5` | `ca236816c859a9c2834598e2e4e67815383c2784` | chore(task): refresh gc scheduler comment |
| `18cebb22b98aaaf49ace89bbf042f2d2facb3945` | `1a3a6ebfe1cd4aaaecf97fb1b446e2b4b48e6ffa` | `4837034252a7c99327dda499bad5605c9a42c0b2` | perf(task): safely reintroduce per-cpu stack cache |
| `88cf1791ce74feae9544706b1e0b61720d210b29` | `cf5dfbdfc3220f9eeb7451b365ea4a28b8bc5dd9` | `119605f62f8cd435e2c078b0938423442abff4bb` | fix kernel affinity, futex, and runner syncs |
| `e61486c1d2eb4d14a92bce65af6ef999307d7141` | `f7cb4dc4e0b8a850de83e6e20dff6f175a8f3cde` | `b23a902c24ca95f823ce20520919b7b89ac256f7` | fix(futex): cancel waiters before drop cleanup |
| `d583b49738dc1ba08a3814f171f0a6d0f9bba3b2` | `81f7aa7940cf8463c3738104ebd4ae86c86ba3b0` | `6eeb4eabf1456118c3987b2397025ca82711c63e` | perf(kernel): reduce low-memory task pressure |
| `5a3b0efd17f4f88f0add0ad081bcc923767ad8fc` | `88265f4db966c650696d49a962a4a8315cb11121` | `d209969a3ba15c849b99f8e7fd35ba7810acf4b2` | perf(kernel): shrink rv task memory footprint further |
| `f32b5415e486b4d80beced7153db087a75034917` | `d49174d8598252b2cfa0683bc21a25429dd0a687` | `6727bbe99adaaf266d2e73da29c35a26b3ff9f5b` | fix(kernel): restore rv kernel stack headroom |
| `1148158b2dbd0713e3a45310f4b3225279fddc6f` | `08936c26e04a78f8cb5e9778c34316b64f9946d7` | `254da63d65e5c5842b5d726a6c3b028b71bc2c38` | perf(kernel): skip redundant cow reprotects |
| `faa7fe9a05d23dbea3b643d3a22467553f9b23ae` | `935aa94392eb687af2a12371b45f822c955e7c04` | `3612f70bff338d5ee8e84589d62bbdf77f3414c5` | fix(net): preserve tcp recv semantics after peer close |
| `c7cb4cc5d4661f24402b5073b0a8cc541d3cfa2e` | `fbd4dcd617632e7f8cf2618247c2f3669afa871c` | `2198695f045681a1e6847a2e53d995f5bf9fef54` | feat(proc): expose kernel tainted state |
| `1bb44bfd3f7f50c54b2e0d0f8e9ff649b6db0d4f` | `e997bc87b53005abaf4d365537bf076742d9e332` | `44c07cb4178169fb9760da4be098da66f1b62add` | fix(net): give TCP close a short drain window |
| `2ec91286a5f99678c31407f2afd7c7b85b45d995` | `326f9ecdd7c6291c6c20d26b15af7ae45a5bad02` | `68ddc10037a0af2bfec51a7247207d50daa5818f` | fix(net): yield during tcp close handshakes |
| `7e50557fd0d08797091fb553620c16728fbb6346` | `a72b6c60aade947deaa6726a91532e819e29081c` | `b5f7a54e94d909dd23644019952d9d99674307dd` | perf(mm): skip tlb flushes for inactive page tables |
| `65ef144aa2ddcb01feb6ed7062177d6fc6eb9190` | `0c3c645c6e1bfc581e4bf59765ba5453ec429c43` | `7457e6c87461f38a270d884598a35f6b2264d652` | fix(fs): zero file cache tails for partial pages |
| `15a2c0a388a94a5f67d854df19bb4ccd4cfbba54` | `3c81890690437f4be06ccb31b7f421c29c77f273` | `4f92457f4ce13e754a6a77fd29df6f143e79fb91` | perf(mm): batch cow frame ref lookups |
| `b3e60fb0ba82bb4230e8e9586371533829e3f725` | `bb59a6b95647a216d300064ab57a53ad92d9e48e` | `3faf2b1d9bf69e6efd0fc1a0d4d3b403b443489a` | Revert "perf(mm): batch cow frame ref lookups" |
| `c661bc8181b1261046339913e5a32b3bd215b2d0` | `bfdadcb1aa40cf4d27babe4afdd4ff1c6a8b2230` | `61becfd9dfb47e1675bcda7d8c6a9f7411455caa` | fix(loader): align auxv layout for static glibc |
| `c62723f07cbf18137ec3f63dc8a7e9c77cb82998` | `6f5e2c84615efb9eedb19d65cf9c29818781bbd2` | `254aa73a0dcc3a02564200f4158cf6114bc337fd` | refactor(oscomp): switch to official pre-2025 images |
| `06d3ab3c53f174ccf0469fedce0abb0764d88e06` | `26e808c7350754e562d930172bac23b8a0f79912` | `5addbf0e66ddb2e68e2f09b2ae9d06cedf3b00b9` | oscomp: align pre-2025 eval flow with real images |
| `138676b4838b29d9c5d4e1365290c1a8b4a719c1` | `33476068a523ef349d9d000a1efd429c80ad0586` | `677a0622fe401bd5c72a6bfa3aea6e5da7ccd811` | revert: drop oscomp runner changes from main |
| `6b814deac8728bfa52dfd5f98dd18c16ac511584` | `a05a90bdbb1e008ebdf40e4809e85f34cd93fce7` | `1b61d9c6dfc099cbe51904082d75d2e140f058c3` | build: fix remote make-all kernel targets |
| `6662faccb513c66f7564651f4ed1c2c645a0e3a2` | `abccd8ec0713bb2569f6f272f02119fbf39d4ae5` | `28678936b6a6f64a6efec066455b262c83ee3553` | kernel: implement sync and reboot syscalls |
| `00c505a35804f9b891b7adb9c6d1b567c53914ee` | `ed3ed22936cf572dbdd16200d722fb9848944650` | `a50b5b9d4175224316055bd70895f4803c303669` | kernel: harden cow and syncfs semantics |
| `e70ba3ccec9a49678993c94c643db5af1ef2c376` | `98eb84f0f832622eda534ab7707f91ed3693322a` | `9e548a230aa4e91e0fb87b8c02cfbbb3b13dda8c` | build: fix riscv evaluator kernel and runner roots |
| `efca8cb337bc145fed23fb835c7a77b7cd9b234e` | `8cf7f0e01c7f045be46fb057bea23e30388de3f7` | `c4b83808e0dda6f631150113fdbcc7a8736e74b4` | feat: align evaluator runtime and expand syscall coverage |
| `42ee4791e5e04a40ea1b7adc0a86043e604f143d` | `8679fce38d86c95d572dc00e3daa80f26ca927d1` | `f8c3d2cfe5aa5c9516e0d5fbed421c193b083e48` | kernel: make robust futex exit best-effort |
| `69bd3157956d68f1d3c75798961cff13b7cca72f` | `da2470170bdeae4eb0b74d2cb8b31d84d9742a11` | `9203b922c0628aa49177a1b703eecd1c07d309f5` | oscomp: align runner with official pre-2025 images |
| `ec5f8ebec2bd98e369a19983a99172e797e2e66c` | `f6626e8872e1a8e8447ca9a690424931501318d9` | `20c1c364a6c57d5a049c655488de5d4ace79cf06` | build: emit evaluator support disks in make all |
| `fa859749349fd3c8f5ee15c64ca816ee45f793d2` | `ee9f29ef34d96f6652e3ad50df6858928bad723d` | `fc6225a4d6048d72384d532895980fa7eedc672a` | build: unify evaluator support disk as disk.img |
| `8c6ce3ff75ff82e6f930496c4b6a48ec25e92b1a` | `d47829b4752c772b3127510a812fd2218da713a4` | `d52cb6a3d1d6fc48f9a0c3157c5f7ab363b98f57` | oscomp: align evaluator flow to T202 references |
| `efb58d5b4e0189480b0672e7c10d8bc8c51cc1f8` | `167dc83c0c925a5fc39926ca35ee3ce0b62ad40a` | `e5327e6a7af992ae3c5d55002344202f909d1b27` | fix: stabilize evaluator runtime and access semantics |
| `70f96a196b52033b2d8b18d2aaaba9a57963b9e2` | `de44351f00457cd945ad3f577eea9043f8855ff0` | `1cd06a5366ed32a063cb5d1245933a43656b0940` | fix: unblock evaluator build and rv ltp regressions |
| `29afc5375c647fd8400231d9e7d58030a7298fe9` | `60492ce01852c01729e3e3c38c80d10d9ee1f8d1` | `4968cf893669e215d01d98ae6ef8741859c915c4` | chore: rebuild development environment around repo-local docker |
| `01e5029b3bb8561f46c6a2ef0883c161c0cc4e23` | `73a53222681d33a9b3e4056f5e529cc563f2b033` | `48486bc5ef1519509924c53b10c1ed42032e11a8` | fix: harden dev image downloads and unblock la bootstrap |
| `c9ef4df99f7998b085a530851ba54a1b0e15846d` | `2d279d91c91a7879299f1e663bcdad348639db9b` | `bafce29e1c7d1c8e26a5fd4d43ef42548ab1dd8e` | fix: unblock rv oscomp path through ltp |
| `58ff41232fcbb07e9eceda21ff25188f561ef14c` | `ee45a0e339623bcb89b0b56fdcee35765fb24e7c` | `d8e5a22dc94c411c6306844858365121cc0ffba9` | fix: decouple evaluator builds from cargo-axplat |
| `8f074598e25ab9a6dab2b612da6fa05c024f2f45` | `bfcf62761eb2cf53da2b9c962486b0f86d2bed7a` | `7672fcc252f85823156283ce803a73007ed5b8a2` | fix: restore linker flags for build-elf |
| `1b082de0bbea46deb7fcefdfcf7f73513dc9863e` | `80f1c7b850040358a99ebffb0510eb6823cae13c` | `8d7b13f806953873fc9135fc5927a772b8afdfb1` | fix: align oscomp runner output and support disk |
| `d6e030e1235e128908c0ca1d14ed34a67399bce7` | `82893c2f79824ba1fe8f76fd059822e7ee8e5a9d` | `5937460faf2ebe3713b09af90cb4aeda7f0f52b0` | fix: align evaluator output and quiet default logs |
| `5306a3b03cca0d7f0c38144224a0f109e6f05409` | `35db31f1e16cb5ae4a7fb91afa5a26e07d3a8940` | `e80699a817b810623c775e0f21a54a87add96cb0` | fix: use distro cross toolchains for lwext4 |
| `db6591936b53acd6d6ffc5b12fbda3d0e25127ab` | `4bd0a7c1295c34d90e40d36da8355ad933df2545` | `05f30ccb02eadac278c29909201987efef363436` | fix: harden oscomp kernel semantics and compatibility |
| `6064e032fcaceffde4332abf5ed2c6d4f9722e25` | `44a56d76cd56324fbf1f1f746684ad16f7ce42c8` | `003c42eeaa8ff76e52240c57b8534ccc3a58017b` | fix: restore libc-specific oscomp group markers |
| `faa1428af34f1774fef865b74595ea1e0b26f0ce` | `768524ead090ab1ddffd81f833ba4ecc50becbde` | `672410e584960bdc157cc632b261fb3114075a52` | fix: unblock more rv ltp compatibility cases |
| `7f6e3989ae9616600132d3f9a59d700ad1bfa634` | `77e699d7c8c7a24d01947769973df5b564a5e782` | `f2cc9367a7a73e77e57686021d13234cf0939482` | fix: improve rv ltp syscall compatibility |
| `140727ae2ef35c56a99b20229bedede89d176099` | `cefea24112c0ff5b8e58905e9efa4d239daec975` | `a22abaea9679faaf3cc82f8f3860acf6edcac7c1` | fix: restore repo-local make all build parity |
| `e80a8639718401b157ef398cc41c1b0796ce0747` | `e6f38faae84afc5f7236dba88fee20d42625cc3f` | `4490fb0e4eac504d831c25cb070523d6ae344cfa` | fix: complete rv mmap06 mmap12 and mmap13 semantics |
| `04f30c69d8b36acbfd1caa9453a2f48fb9834fd4` | `4e90b4d0c7c28c6442f622c887d28a46f50654f3` | `b4cdb5a219f9cf3041949ba241ad97f6c804a4b2` | fix: complete rv memfd_create semantics |
| `1feac46d2e26d22b638c8220773367b0c9031801` | `d9f13b3b814155e2903a09cf67e9fdfc05a4134c` | `1ef0bbba4f3c17e3353f9dcecf642c18606f47a9` | fix: restore streaming ltp evaluator output |
| `d7245aac4353f2d5f8c622ec2537dc00c462a2b4` | `46e983c95e2c9e6029d07c8e0dfc0700859925b8` | `543e96d663604d2ae9d6e6993206b0f5f9eba45e` | fix: complete rv ltp clock open and recvmsg coverage |
| `7e4fe2792f30e27771dad38e90061a28e825a2ce` | `fa18df2e4f0123c94109913de214ab8030565293` | `e81079670e0a4759edb5e1f3817bb15c399dc246` | fix: complete rv ltp mmap18 and recvmsg coverage |
| `b24ab1b5fd091a3559b5ec88f9243539629e8b0a` | `ae88c37f5bb1a6c62c34e0508f36148387755024` | `86fcc75ecb1a3007b8f4a453204d5343e1bb2b65` | fix: handle libc mmsg prototype mismatch |
| `afe3d032e5fca03bf536d1bae745b0c91853caf8` | `ecf40d1d489ebfd69c7419832fd7915996923808` | `1d45d1e76b651e027228642175367516455c8cc8` | fix: pin smoltcp iface addr capacity in build flow |
| `dd387678d596c837c0fb2c4ab23886245bbc5235` | `1b840e1eb442c8ee88ee49166f3fc1b90923fcdd` | `4d243ebc07f6872fb32770331a2383e94f47fd66` | fix: harden more rv ltp signal and mount semantics |
| `549d07fd50cc19b54ff6714e488fc12a4dcd0384` | `f6fad12c8a7d8e92ff6f877d0a0654e521d930ce` | `330299d026cdf6e5054906c8f66fe04074eb68fd` | fix: harden rv ltp credentials and scheduling semantics |
| `23568100f0e97af8618d40ef175c066e4b41f782` | `c020551624326780f5fd0ea8cda075d4b8a0d834` | `5e27afb5f314b3ec069e832c8486664976b0af6d` | fix: improve rv ltp socket compatibility |
| `e89f354db83a55135ddc86274dee6bebc7e4b8d4` | `5d832ab94c1b3e18646c13de4d79b0c8f34e05ce` | `8baffff6752b6ed12f89b5bcff4b181a58773d32` | fix: improve rv ltp splice and procfs semantics |
| `6d9f8dee3dbbe9a8d339b6a2a7e03a29a4cdc268` | `5b789b2cd9cb180f3732a12ed5e53005e93f1ef4` | `2361652ae283f58c3be87fd5b6986ed3d2cca88e` | fix: unblock rv ltp syscall and process semantics |
| `a424f821455a1b6eb52a8e8b7b867494006bd141` | `af921e839f13b23da5608edb7287c29861f8a062` | `abcba9ccef47cf4cd9b9083a4ee4c5ceedfbb6b1` | fix: batch kernel and support compatibility updates |
| `97457ca2ff645386594b1134d4ba6244d92b5a86` | `3be7a0cca39bbc33e082bee789bdece22c9b810f` | `6d982f9a57f4ae46f93555ed10f3c6a9389d371d` | docs: describe local oscomp flow and autoscrub builds |
| `91ba1b1ff49bf3e6020920e8f5ca29ead6221d68` | `abff610ab992f656bf887f3ecd2ca5cd1980c353` | `2aee66603219c493fad33d0e8f8734b53c5eb925` | fix: stabilize oscomp la evaluation flow |
| `9f0b29903d0104d61f965c048db507eabae90d73` | `2d079bfe535c093072d8b9a9c54eee2e7db8394c` | `68a7eb768e24fc28ada20c4e8a055e74f0813e53` | fix: provide la support disk for oscomp ltp |
| `a2f82a46bd066d36dd8f743e78fb208fde79d8c6` | `7a071da74a560c0a79a6c3e24fa274a05359c9e2` | `9f6e637e21052a0fe4e194515a5a5492a2329946` | refactor(task): split mod.rs into thread, creds, jobctl, and process modules |
| `723a2dbe845508fe3b007042cde55f19958fa193` | `b9e0255a2428999d500e8e0375977ff5aa5db8d2` | `9fb39371e582e958b32df1776e04629943155e4d` | refactor(file): split mod.rs into types, desc, fd_table, and stdio modules |
| `cf5c178001b1dea4e6fe18f9c7a21355807d362d` | `4b00cc2ab3cc64d72b4fbf4b602bdc7a3f729e32` | `ec1147d160371fc2d69907881d3fd570095fea78` | docs(vendor): add VENDOR.md for 10 vendored crates with Cargo.toml.orig |
| `53035317e3f4d0ea95f7879250a07835f94ababe` | `b3ff87f92bee0a9db2cb3ef6b0f389e2413fccb8` | `7ddb77b528e8cc3872cc8bae63010756487860ec` | fix: address refactor review regressions |
| `e8bdd570687143a948f53a25d4570e01980be50a` | `fed5503e4b0c1b71c68b5c4e393ba5c502ac8050` | `667a5f074a059f97902575fb2dc9c83026c674a9` | perf(futex): skip aspace lock for private futex wait/wake |
| `4d9802858ea383426294341215635f0e12b19cb3` | `b6b719c0a03bb8702b1f58e309776c2f089fc638` | `929d1cf2968480b00d92ee346a1a163f8bae63cf` | perf(task): encapsulate exited-task ops with counter for fast empty check |
| `1a919dda84def88a909292954a55b92a588c73c9` | `4c27cd5c98594921eb86ec8bd513002aa6e57caf` | `4f46376316ff92e89fc8cf77c046dcf6c8f34809` | perf(fs): remove intermediate buffer in writev |
| `d2972e8c4a67bf2b33289b1298319d07e5d7ccb9` | `06e35fad4e095d67fa2fe26185126a097efb3bdd` | `e57d02d890df14b8003156a1d96ad60f14150971` | fix(fs): prevalidate regular-file writev iovecs |
| `6e424ed7ffc0c678d17fee25c6a42f30cab1b368` | `68349c8989d6b52c7800d07367adf6960932afae` | `c6eebf9af45861706ebabea0fd616ca16cb0c0ac` | perf(syscall): skip socket timeout check for read/write/readv/writev |
| `c1a604e5ad94c482a2940eeb385609affe1a734c` | `f661276b478bec05229050dab5ff298b37ac654c` | `8f59d63f0a35677077a8ea32de17d81053a58436` | perf(fd): add FileFast enum to skip FileLike vtable dispatch on hot paths |
| `ac29a16615b283aa84c23dbac7ecc23599838c47` | `8287df5aab4e45d8d05c37bc0dc819d3c7f49429` | `ffe9ac3ac30718d62543ef30c0e8450746ccf3f7` | perf(futex): use private key for clear_child_tid futex wake |
| `2152ec2ea16c9dcfc34fb1c82baabcb85f9059b5` | `c791dae04606cc3f2dafd0333bb671858e90280b` | `045238e8891218d93bed11e59e2ef48471c90d2f` | fix: address review feedback for fd fast path and futex restart |
| `3e21ba25890a65a47aa63243fea7fc0ebc40388a` | `455cb275e489d90912cf9986d193f0cda861b5e3` | `61deb6ef4bec8f7ed56fb510f7a7745fd25bd1be` | fix(mm): batch COW, loader, mmap, and file cache fixes |
| `549473e2dd6af0d567060723cfbc870771cf0a57` | `d369a72b1ba09cde8b3ead215a90bd9b4203141f` | `2c92dfd7ab09b8a4b61c42891e587d4f9455ef5b` | perf(pipe): add fast synchronous path to read_fast and write_fast |
| `5928971ba0c4e9269a6ace74c7687a28cc2a7ab6` | `73f1f2f21fbc45e8a8c2d58ab1a46e7710c6a54c` | `8bbd329739ebf90c02fba9b672c9e3e3ee992bba` | perf(mm): shard FRAME_TABLE into 64 independent slots |
| `72e4f5568d3160514ede2559d865a3b272b8715c` | `9f3584e68f114de63e9336bd8c12fa815664083a` | `a6899bbfb46d4730d426c6a19429fdaa082653d9` | fix: preserve dirty flag on truncate and skip sigbus pages in clone_map |
| `959179574f74447b589ea566bb86f16d45be50d1` | `e61ae2f6b909d5d3d1aad0944140563bc2c87302` | `bc4e631e7cb4e3fe047f05d5ccbf14ffb1f484bd` | fix(syscall): defer socket timeout check to EINTR path for read/write |
| `0eef861aacbd832c75d3d09c2864ce176fefe52f` | `1d352f6d50269f6e29624b7db95ea543cb2d724c` | `96c5b142c1710ca872e5de7de0900162668dd9bf` | perf(task): use thresholded reclaim in clone to avoid ping-pong |
| `dbb93464d5bd88b6d37db07d1ad8deede11078e3` | `5242dbf01bd60647908baca10b1efab2ebcccef5` | `38a45b241c8442e46b61825fef045aa1448d4664` | perf(task): add Thread object cache for pthread-create-heavy workloads |
| `bbef05e5575bba533fe95c3a9f27ccf66b5419fc` | `cbd705b36a82d0421c41a6735173dc55c964a2dc` | `af59bd2ad32fa7e8beb3c1ba8ba63660c8ccefda` | feat(page-cache): add global registry for sync/syncfs on close+ dirty caches |
| `02a2ba537bdf855536710e241c3932c9d7dd561d` | `40190e3b5dfa70b099008ed0a183719f5f9ed874` | `94d35dd08916b3a92911ef5c41852661db7587be` | feat(page-cache): writeback dirty pages from registry on sync/syncfs |
| `6714551de28737a71e306a8aa0687c391df7e0c7` | `133a2a61e49a9f281848adad1239ce00e5e0ee68` | `5017136db2ba021b843da1fa9281b0340e0fd4fe` | feat(page-cache): coherence API and global clean-page LRU with budget |
| `9971dc6c05794a9cf7df204631c3b534e803e499` | `599b0d9d8b4d48b1623ad19dca0dd71d010291d6` | `887e9ce5db37ddc2bc14671a3b4c11b5892e57f7` | fix(page-cache): wire coherence into truncate/ftruncate and fallocate |
| `905135bf20bf3d86cd223bac81dfd2de09d75640` | `1818ac09473419d72491c628c4a576ea09737cfb` | `0c11e1ecc57fc84417b0dc3204a23a3f14050de4` | fix: address review findings (pipe close, thread cache, reclaim, flush) |
| `cabb78fdfbd18baa8a792dfeeabbc3c47de0817d` | `bbd61e182be2f65487bb173c1af8dfa6dd9b1b13` | `bbe37473da2daef16b285db62258ec86a07e57b1` | fix: address review findings round 3 (build, evict, tmpfs, COW, cache) |
| `dd4e860d83eeafdc290ac31b0afb5576c3f9afa3` | `15eac33d7c0935d8499dd637f5090457c0612669` | `0afdfa761debece4690f4109623c507b5a061873` | fix: restore buildable cache and thread preflight |
| `76f1c5a965918f9340d35f4463bbd33af5668158` | `643e185e1197ab6164a093952323debeb1269fc1` | `f361728055618d27624ef0f740f9bfa991fd6bbd` | perf(task): cache reusable TaskInner shells |
| `95e29cc4ead9e485b59263c70dfbb6331ecbd688` | `2cf8d96a7e54cdc10c19fe31fe14018bed500aef` | `4f6fb0112daaa87df1984fffb7e624bb2d803c5b` | perf(pipe): replace anonymous pipe buffer with atomic ring |
| `2009247098057eea99dc556c5697622beedf80e0` | `0c0eba9f9b70533647cd25f15bba1bc7b227b565` | `5c404d3a1dcb38756ef6f41055211517238ee07a` | perf(mm): cache hot VMA lookups |
| `4be314fa544d0aa946d05cd316eae7969f69cd46` | `2136a6a1803535d9e78fb7ec725bdeb2aecc84c1` | `9ab80da331ab52b717643cfec6ddfbd3aad8d596` | docs(vendor): document remaining patched crates |
| `cb944a589752b22a33afb55647c787dfc179f28e` | `9e20d89b5620ca3efc804778636de346ede3df4f` | `9ea042f7ba4c74b5abfd802bdd5b33c17bac1542` | fix(runner): stream ltp output directly |
| `4a362e6cbe7c1da85e391d419d42f617224c4ab5` | `370b5fd4ebbe7dcd63515a8cdbc6e76729037f24` | `b20e734381994854bebd45248f7a7e3dac342442` | fix(mm): use safe snapshots for page-fault VMA cache |
| `4229a0b919061c963c731380a1064b875add047e` | `34d983a04b3c434deb9b7332d9327b48fab41f89` | `7bad6ff47beca40ad7d2fdc4ca0f3a431291c4bc` | fix(mm): guard COW against foreign huge leaves |
| `98fe66eca3377194b089049798b9747dd21db66f` | `cd8db7f66bcb5c6acc124db71f7da30315275894` | `fa464568c1ec4c3d3711a754cc32458820863671` | fix(runner): keep LTP native output off evaluator console |
| `630a5f64d8d854054b62ac00e0bc210a4496f2c0` | `d0d4eb37ade2e2997c603918f8d4a839c63f0ebf` | `6bb072ede37cc8366120d9dcafad1b5b9f489a88` | fix(runner): avoid LA output capture hang |
| `7d43c7aefcce5fd5b115f6155a5803ba8c9cddc8` | `a5d958d328eb23bb9ceb00e15036920e16a09aeb` | `0f2deefcf6b50578fc9a8bb241fddcf15550dd7f` | fix(oscomp): normalize judge-visible benchmark output |
| `222abf5946c6a6f53329563d4fbc88518aaa7ada` | `66c14a0415e1083e6588f63c286e1be0aaed686e` | `1db8373f13d33da06f6bd057c0923bdc5267ee0c` | fix(oscomp): prioritize ltp and stabilize ltp runs |
| `754f63ed2b91299af72efc98accee2be3846231e` | `63fd387afe57a8b20b3a85d8500e3ce130d2e279` | `d716420ddcd0ca971c78298f5661d5bbedb732e8` | revert: restore pre-large-perf state |
| `0599a8cf89ee7ae6dccc40c1ee0108291b777003` | `e0a1e18bdcb3831b59e7105ad379da56ee0c17d5` | `a8f53b478089b06a46788754fd3ae96fdb7e9799` | fix(oscomp): colorize ltp output |
| `9a50a61707d735ab6714527f6190cdf25975c5ad` | `a2976bb3e652bd04f8fec0a7183fc78286412a01` | `a9c2e6f286d3dc6b5f0304f31d51fc5becdf7ba2` | fix(oscomp): expand judge-visible ltp coverage |
| `05e8633ce1d01ae948d467acc2e6dcabad5bf197` | `ccc8dc8247176ce8e805672e01250b217032cd09` | `8afed76dc82c342b29b54b5b4bd0cff15ff23dee` | fix(oscomp): add verified ltp coverage |
| `37fa5cfc7a5815cffdff3bcdd6338f4782ae171f` | `1d3deaea82d749531f40baafe86a0279fc6ea47e` | `de2e7f96c0035255bf3781c06551a489eeecf044` | fix(oscomp): add more verified ltp cases |
| `551e4b02793382953eec2f0c2545bb4b696c18c4` | `bec1a68af0b7bb2d8235f7e19391f3f708f4fde0` | `3641eb96f3ec6429127b8e14059ce5fc981db4d2` | fix(oscomp): add verified ltp cases |
| `b35ef46621e05257e95f6dc85e72ff0c7b1426fa` | `c3292f5f13ee56b17431ad8bc8db763a9fdab20e` | `a89d9a1368eecad4a13ae1399be4f90701c5d145` | fix(oscomp): normalize legacy ltp results and add cases |
| `b5f7a752f800f536639a9c9f82081aecc7b0110b` | `11ab229f597775d2bcbc40e6e83c62b30631d566` | `474a8426a00a99b69f23b4484cc2b14ad1be8a98` | fix(oscomp): add more verified ltp cases |
| `c5bff3508f5b9a3e9a18137e1b02a6dbb6d5cf5d` | `b3c44c57fa67caee5671205cd480debf14520d32` | `9e6fb0421f6ff74f5ff4dab21e6a03658b87d94f` | fix(oscomp): add verified memory and pipe ltp cases |
| `a2ceb6f2bc4b5d7d18846cb037197698269494b0` | `945a43176f1a0f86b4484cf0823e79ad637e3098` | `58124cc4fa358a0fde5ef02cda03c0fb2490a38b` | fix(oscomp): count legacy child ltp results |
| `75b018a0c54cb1e67100f8475d91958555902cb8` | `24df69356fa9c35d15e6cc3950267114cfb24f9e` | `65a0a1a4eb373f2f5770c013a5c039e29e0e1f68` | fix(oscomp): add pidfd and prctl ltp cases |
| `ef46c23e95a3658f7529af1be32dba35cfa3e288` | `013c69fe73e13fc4a31a0f7567249f9ec7f9e09c` | `323d1a3467a575ead563f09e2bc85e47b105dcea` | fix(oscomp): add epoll ltp coverage |
| `9da2267d134176a216c56dd203a9474f82456f67` | `7c5a45f96d2c1883eed1785c1874a1a08d862556` | `2f3832c8f3f433a9641ba8e1094375c53e3a9b80` | fix(oscomp): add futex and io_uring ltp cases |
| `896b598c3ae27aa403e267a762bbf5a6e32cbb30` | `307bd095843e75228de5f2b09bde0cc44f6aab18` | `21b97a6378f814d9d5efb90fcf4d3182db9b6370` | fix(oscomp): add leapsec and mremap ltp cases |
| `8b0325eabd9d1b7cb81131e7a6764d8b88cd6cb3` | `4dc3ae6becccb2dd944bdb3b83d50d309c83b3d1` | `37ec8332ae03b28960d35f7de1513c3794041007` | fix(oscomp): add ioctl block ltp cases |
| `6deeaf4921115b33bc0de43be667c42500fb27dd` | `298315de47eacbe15963d75bc593d00bec36b946` | `744269bb2085f187c3dd68bb034ed75cabac6cbf` | fix(oscomp): add misc syscall ltp cases |
| `af2c7685424c050e13d94e5414ff32e9c56058af` | `94eeb4249f8714483ca3ae02ab8ae1467310d77a` | `3ca6a81d990286a0fc80584d135bad8366b07d9d` | fix(oscomp): add proc sys ltp cases |
| `2dff1aab128fcee89349558a116c2fbd1b808b0d` | `f061d5a665eb7388c76e4f01da6035e1bd646ef6` | `dcce99aa990caa0cef75152f05878bd21dd842a9` | fix(oscomp): add inotify race ltp case |
| `af9aca4e2342010ba4d72e06bc52157f9f6d9d17` | `083b4d707482bd2b685eec0d600dd83db81b2423` | `2b8c94fe35a79d13fe0b3c9d40d42ea8e3f08589` | fix(oscomp): add prctl timerslack ltp cases |
| `b7a91f673215c7c03beec1ea716e0ee953e8dad5` | `b347861a32b9d1acccfb8e8fa088f13175c13679` | `f929f0e0eb76d732bf60e80ef18686f188e07608` | fix(oscomp): add pipe soft limit ltp case |
| `4fee8fff1df749605d13a4791205ff2f8a8d0c51` | `34323d8f48d5b5d07700250835dc7da557205578` | `0872556455ba50087909b7226d4d06968ddad260` | fix(oscomp): add pidfd proc dir signal case |
| `c6f15a8dc6425ec5cd51ac8ca380497d544de282` | `52616fdbdd66a295adb5387fede2539946636736` | `68e2a771f1422fe2342ba7f310f0552a36701482` | fix(oscomp): add prctl timerslack ltp case |
| `265eded1023799e6158937c924cd86031947a7a5` | `75c37f07b04b30d33836450a4fcfe9025c3293cd` | `c2e264cd8f58ea09c06f9d3f5fd0255e5ad3083b` | fix(oscomp): add ltp mmap and inotify cases |
| `9891b5d31c77539b0e5b02a003d429c95732ea7f` | `56782d0ea442d461a0984fb6a612e642bc8d4ef5` | `d92cfb9547c8b69a4876f0033dcecda7692c61e7` | fix(oscomp): expose silent fcntl ltp passes |
| `caca273cad0b78500d83dad6accbd28329ccb239` | `af8ebd22681ae7f8d247e691d25da292d6b1cdec` | `baffb2928bcb3b8ee23cbcbe8e3a0f7178f77e66` | test(ltp): enable mmap03 across architectures |
| `20625cc2a324d1355231a93b57c7a4b9f7e28d32` | `c3d20b26bdeb359ca9011315863bf987222d9cbc` | `49623a70191eccb9d55abcd49ca022815dc49b6e` | fix(ltp): isolate UTS namespace hostnames |
| `9c7b830c112c06d722a8cd7f6f54a977bdae8586` | `c06f835438305d0d1e482e30112293e6aee6954b` | `96229dd6fa06304059e61188068c7ba77b886e62` | fix(ltp): support Linux personality flags |
| `e5f0d9bd0f97c64d82908929a691d7c88244e690` | `ca42ede97872c7db25accf881eb4474439e07e2c` | `8889e38f835983157d6dd3c74f1702cb85efd9f8` | fix(ltp): support file handle syscalls |
| `4a322e0a7b9bdc0747f43b68128e0e8e0e21d261` | `7d55f9fef12dd1e5345c623e5d40f33137f98282` | `038d1433b18bac976a2d3c5ec6432c0a4f21b450` | fix(ltp): support mincore and pipe size cases |
| `2b13862b6a57f0dcc4137a7af8573055575a05a2` | `4aff345c149faa88612361029f78003def4ee581` | `ba3d13928ec83e7d76cb2378168e113215ceee19` | fix(ltp): support anonymous tmpfile links |
| `f0d7b101d1128ea26d8eb33508a83459a2a6370f` | `e89370b0a2464825f18a3cfda668ee917cc0e076` | `7b409da60af49ed091ce42f09e673ff6952d3f5e` | test(ltp): enable tmpfile and rename cases |
| `9305e10e1e1dd25ce86a37a5fdc572a66891a92d` | `a5768f0fc44353409bd304742193741f24c96a5d` | `550560b0b1f82ee7c903786397662ea1f2e806b3` | fix(ltp): enforce epoll add constraints |
| `c64efc10cb79448ed29eacb82f2325b7ebfa22c3` | `4de20cba3444280981a004f98d85211f878368b3` | `bb0e1cf404716fde41e7d33a876a179099fffd19` | fix(ltp): implement fcntl record locks |
| `85cd804ee8c9dae175a4b5d1d3caec7e0d868206` | `36173b2718213f4dcee76081fd1dbb0feb3087f9` | `1c955564d74c3770eb80749d8e1e0184a85d3af4` | fix(ltp): detect fcntl lock deadlocks |
| `e9951f49987b880d0531649c65b9c9af05962fa0` | `22c5d54c1bf3608a6d44b977e42bda067ad87ca4` | `88baf01a1f425c6beec592460eefeda6495d22ed` | fix(ltp): support async fcntl pipe signals |
| `5c2a3c456007a28644367ef7cba69892f0fc6811` | `423093ad66d25ceb8aac0b700c72f3953751c85f` | `3c8c09a4d903644e7884674123410145e9f5eeff` | fix(ltp): support dnotify and rename exchange cases |
| `8743acfaf8af129d9c540af071a23110d93f9749` | `a51b9fb93e4e91b5875fb1d4c335dd49b52f9ba4` | `d131dbd21770cdb9cbdbe6cf061b3b23e4613e05` | fix(ltp): expose sysfs block stats |
| `0db844e3b1d93ffa148a6fac054b2bb3953e3620` | `47ff7d73151ba7d7ea34edcc743bf93ebc131e03` | `5222ff0b95b9f4dcf6fdef962b769fcae1e363dc` | fix(ltp): enable nftw coverage across libc variants |
| `e0bc297d3ef212f7e2ac642677b487812c652fe4` | `ad0a7504cbb8b0ff20e2abf8a66e736238585808` | `4836b94c380fd99d0ac8ab37a14b37c44a594b63` | fix(ltp): report loop sector size |
| `763bd8cfd8f91d7464491a893a9a633d3137ea8a` | `09ab8800fcd736a65a40d373a46c9d89c6f2d173` | `484b17161865c5ff15d32ac305058935097fe2d4` | fix(ltp): support mlockall status reporting |
| `26019b7492273f285b2f8e1a9c15f8efb3b00ebd` | `7d36256a6d05e5c75c12f9a8458af6a509ef01a5` | `2f076fba7ecad333f61c1d1626d996326d844fe7` | fix(ltp): use comparable uname release |
| `5d772c71db72e434b1f3cc572ad423f0aafbcefe` | `1b44852d9a767552674974137dfecaf815baf14e` | `ea9023b97d8257ee754100a971fba4d419dfff79` | fix(ltp): support mlock2 resident page checks |
| `66779437d1dd9255b96d2926733b0034911f25b3` | `e7ca92a6a87f8f07291a81608d9d7c4521022eff` | `31ab97d82777bad337338f6036a0374459c712f7` | fix(ltp): support smaps and locked mmap cases |
| `29bf2cfb5a8ab7655306814407104a6e522d25f9` | `362cb8d86335af8bc84d6456c27249263fd5597e` | `3843a0da0cb0ef8a3d724cbbf2161542c1babcf9` | fix(ltp): support zero-device mmap growth cases |
| `5a49f36eafd94724e244fe2df545e66daba5b3d3` | `43ff3ca09a8939c8b7325e79f8d9de3b13b399e9` | `8803fd6f8b4e4dfa8480e6ac35540e19b310df80` | fix(ltp): prioritize bad fd mmap errors |
| `d02ec2fff9ded3211f168d210509c63483ffa385` | `36bd634dc25767e8aecfff5954328326674dd80c` | `3c5b785715c026396fb44dc8bdd94dbec03d5034` | fix(ltp): support dirty file msync checks |
| `2fd5b29b77dbdb941b6635cd7db27c1a6f7bfe17` | `204d631c4f18af94cea6ca861dd664dd861774be` | `c22ba4f4ca8bfd7039a198e568db62f7f3823acc` | test(ltp): open additional sync cases |
| `7c0379c7ee92146451d3f85589adf6b965baa462` | `4aebab166f6563fd53c126f3e6285db446ad4fac` | `08be97434275d32e87e914fb28339bc6e9b91412` | fix(ltp): expose kconfig and preserve tmpfs rename contents |
| `382679a387d4d7e25fa4dbe4c2b203d2e3010010` | `14378210ab8b5efef602118b6f6e10376bb78c63` | `6cc601ce048d4c6c536a8b2230e66cdd11e1c0db` | fix(ltp): align symlink link semantics |
| `484a1c6d551763a9736fa38715e65d139f1a7966` | `37102fd348244f20e2eeedf5ac8ec9a8c961d9e2` | `8c18290810d2fbbc6ed6a6a162fd2ed6af0f141e` | fix(ltp): reject directory updates on readonly mounts |
| `18d53f4112ab36a94b7473a71cb52b2e7085ff76` | `1456608f22891c7833d886c9e0f90d90139d427a` | `fe28f4ed93c88a7ce561e270a8f3a109aeb4dbe4` | fix(oscomp): schedule iperf before ltp budget |
| `2ba0c02f07af83c71314862b781912fb9b5e0517` | `812157eacd362706f79b604bee07f951ce4ebc06` | `2581987622adad725cb7396e9d18355380017956` | test(ltp): prioritize high-yield cases |
| `2604355e43a5b736cfdd49193cd4b2fee9389de3` | `1a6e700653aee8ab67c0b0174c1f25941679d98a` | `052abf23fff0b96cc327ba3e0c32e6c1a8f45625` | Add OSComp LTP lab framework |
| `d508221454986c69383a4e302a219cc051f5fcae` | `59560090272928eaad023d65886328186e014d33` | `047300d9e73780f8f9f6c2051d2aa0f954475d2c` | Refine OSComp build command workflow |
| `335d963f275e8d0583d40103afbbe0e43f861e61` | `7edecabe96623aece9da9a34318a314b670310cf` | `a5ac79374b29dcdfd93d3281f9e1138eb5b554e7` | test(ltp): harvest passing unopened DIO cases |
| `f640d468c1661b7edcbb8998e8ca7ff0ff1afb22` | `8f2e431e5ade1fde230357bf61cdfae7b7f5add3` | `24a36adf831b97d3efb156045b81bd10560b6128` | ltp: promote fs ioctl and statx cases |
| `110a82313f0bfa76f63432d0907b5717e0191f74` | `60ee6fc4c5ca9f940a20d7e648a432be9c7a7930` | `957dd9f3ac47c10ae5a0d7656a13aca95a29646c` | ltp: implement executable text busy semantics |
| `ed70b2f2090824da65eac10de1a524ec1c432f8e` | `c1738337fc728b26fe73b9a70ca690e444432f09` | `a0cff2a0a352dfe74ee8efb6afe79f0197a06481` | ltp: implement open_tree mount fd basics |
| `87a9c45e0960c9ebbf6cde4fbe5a5381279bc446` | `f901eea8c0214a0c79fb2cc8ac141de291433716` | `8c572f201cf7d01f4aab6badbe0e4b59ed91a5bc` | ltp: implement readahead fd validation |
| `a07dd45418403cd80c11f0064ecc40acfc4d9b67` | `af602930da86cf6e0958a328bbcbe2bc02e249b0` | `bec0a0187d6534496c16500c70de1eb790a1808a` | ltp: tighten metadata fd permission semantics |
| `517ee8e13b987f91c40630335826ce1d74f0ad1c` | `e63195bef1c611d88c863f010d3c7408fe594f02` | `01ab1e05ecdadbf6c8daae6f0237302a3b138501` | ltp: support readlinkat empty symlink fd |
| `05e5968073e6fdd859741ae7da35db5dcc1cd1af` | `b9cb2a770a58951a50cae11dd61a98ee2f130de7` | `454e0599aeaf9bf961ad658bf601063e011723e5` | ltp: honor O_NOATIME on file reads |
| `f6af498e0acb824ff5a66ea800ffe9d2e581fb40` | `b32ce93e83771bcf4bace7bd1ab1bc99c1654334` | `4b2c7727868cefc4150323ba74eef7ceee2818ad` | ltp: share tmpfs page cache across hard links |
| `caf900aa3ccfce5047bde5c1c9fb9e44312968b2` | `9dbecc04e660f457fd16918fc83fbef886ae5252` | `4478a3f797320b01c3df5d3bb2725f9c6f92a955` | ltp: promote initial growfiles cases |
| `661c9e15d5525c37003af0ac279b115d278c6a2e` | `15155bb24935e6e04f401c8928198ac707c18414` | `879eefbae03da147a83594eaffb1d0f099ab6757` | ltp: strengthen tmpfs page cache semantics |
| `8af6eb066ad9ae587417d8380566d04276d8fb13` | `d0be2e20d030b905649edd9084cf1e3fb89428e5` | `28977cad0dc80439e606fc6d2656f44e06c5fe25` | ltp: implement mandatory truncate lock checks |
| `d71cc34955f719dd4ed4e4f18b0ec9cd38c10203` | `f6fbc30700396d31eb566a794bb2e683d72132ad` | `168984532be76054d1d07c7e25c62c741dd44419` | ltp: implement mount flag VFS semantics |
| `627c66ba17f67f4013b159d542d9a7f749d3c620` | `28b50e14a965a1df0a5320f05e49cbeed22f7ae9` | `5a3ce2f8cd126338f33565d3878d4dd67e40e0f2` | vfs: maintain ctime for file metadata changes |
| `daf578245cdd0a01de6fa64b287a51494bd7d10d` | `0bc727c07fd7caa2b464961c95d7a3765a3d505b` | `854fee0327caceea635102209ace3b4af26fa11a` | vfs: expose statx birth time and block dio alignment |
| `e8ff04ae77496a4682b9e0205b8102b174f9b54a` | `378d6b6917de2b368c48f12fac46d86da72bbf58` | `abb1c1d17f9f19f4e55f2ccad96e915f01281ae2` | fs: support readlinkat empty path on fd symlinks |
| `620789f9b0ef3b098f39258adb5ffe012be3baa4` | `56e3011af730d00d6897a53feae9412a1f3e5e31` | `b6bc881054a5e3d6ada94246655d94fd6c11e2d8` | statx: report mount root attributes |
| `dc00dac21f92133ccc4230b0a92176be9c46ba7c` | `4a1fe4eed0e9bea83d839057edfbdb9ce353aac8` | `1bb1b4d4b310e3cdb1b5803724ad3e44c25000de` | block: expose basic device ioctl state |
| `f6f027a5cbcb73830f9b6c27ca10b72e907cb98a` | `bed938855595f89d3042ad8bc7715eedc745df46` | `5df49682491505303f4512dcb880275a2f07da8b` | fs: bound getdents64 scratch buffer |
| `6cd72d568b5fc673e5bc4fc0312f1ff32be397a2` | `22586aeb189b6769078e258f135e61d9fa94478a` | `11aab3015e4ba505ea85597724c60103e4f86276` | fs: reduce tmpfs page-cache pressure |
| `7a40d5f0e1b3f53cd5a2702b5b66682ea8d006b9` | `03549542d80d02b33bf73cbf0ba7539792e08e65` | `46171a1fd4b5d8dc78859f4bde07fbe22cce4061` | Add LTP campaign framework and promote FS cases |
| `505ede0752d1a250c82abd5e1d8d45905dd558c3` | `f317849dc5a2e6e50155619c07eee1d2915be32c` | `7a9187c90cf4d6979c9912a7a70a8cd56962a56c` | Promote growfiles LTP cases |
| `1f38af905606135044627a23eae2bf5f3b1d3e52` | `4c89009303c59bc81a6d0a31d25c362e88bee038` | `5c0077fac7657ef2ad58772ca93dd5daa5624ad0` | Promote FS syscall LTP cases |
| `9a30659b44ff0aa93d677bd1206b557492c72ea6` | `f4f2b0f0a97ff5b8586f8f19431c7cc412548cf6` | `f1b38cbde1bc3bd85c0104748175f1e9ef6d3720` | Fix tty nonblocking reads for read_all_dev |
| `1e8d0664bfba296833c248d486984678686b370f` | `a3f97fec645ae8756fd629442c184979c4f997c3` | `50031e80164250b43aaebb8653c79a88c78f80a3` | Add getcpu and ioprio syscalls |
| `ec3b14d4d164db88555c7581d2b724c7d2cf1bdd` | `a45aa41fba8b2da80d7af42dc960a3f1fbf8a0f4` | `720720e9c4ed14e4645f19385a2e6bcd87ad2781` | ltp: promote syscall frontier message cases |
| `e6c643753f240769de5c095d3c5ccf3a8d2a9dcc` | `20319bc4c799a217f70564b85652b3f05f115856` | `a585cb2bac4576939380053481dbe9700896ac7c` | ltp: expose netns proc sysctls for clone09 |
| `0cf1181b18a904a7717d248e5475e1377187706c` | `8d72504ce334bcc65b9f7d3bf6b709edcb9b86d1` | `f0827ff4c183548f11e90df38c60b059c7a1f03c` | ltp: promote nice05 cpu clock accounting |
| `43c7ffcf969659df1c67bad751b6c1ea95b12508` | `f5ded50f345648239981873fb3df57dbf0e9bd4b` | `a412227c031a1429b7ada87a263a2f100b013547` | ltp: promote inotify queue semantics |
| `31eb7013aa528908853b227d680b44684a98f6a8` | `670a59eb1e8dc5ccf6c37ef0677385d998c65c11` | `ce5866efce716a2230f61341a38163d6a3608608` | ltp: promote fs stream and fork passes |
| `78b2305476416a68d540e4628af23abcaab92251` | `ae7dd5bd96420e9aab10df76d3c0f721325f7bc0` | `7e322d643587677245602f1255da461ff06f45de` | ltp: reject exec of write-open files |
| `faac28d8e60258d5a252ff655c4ec841802f25fd` | `93d90e98de4c59f7f830fe6d2f00dd660b8deb37` | `540bd772a32dac9893bea6c9ace623129ab3278b` | ltp: expose proc namespace ioctl probes |
| `7658d42a0ec28774ec8e6312f55c8ad9b9fb777c` | `6c5fcaa8fa704c536a9f68e255af2bc0c3598424` | `6f77a0ece76b29c40093c4d9620559282e2d0b9b` | ltp: allow pid_max proc sysctl writes |
| `192c866f9b84002622461459815ee44ce30bb54b` | `8529e1141bed84daa906061d254afce905f8828b` | `8cf9e39af32272e6a63a04f2d2acc4d6d54ab27f` | ltp: support tcp addrform disconnect reuse |
| `5f9cade8431738c398863c86775c7f0e89e7bce2` | `3dda64ef38ccf2ec43f823fc9926c4b9f9cc5c82` | `30546df4e58bd6851a15d888a8b6fbf660f158b4` | ltp: cover mount context api basics |
| `288074b94ad7b45a39bab5580ac997cf41d6bedb` | `e7c5a1ff0157ae15523203ceaee16d248070ae02` | `620612530e9989ad4a721f2f0d2ac9a8812e4f7e` | ltp: support child subreaper prctl |
| `4e91702fa42991ed7a8f775fcd8c6e34ec9b1236` | `2dd9e85333e3024cd4458f470af2dc31077ffb4d` | `361e8659b586bc2e423b68b0e598190ca56bd63e` | ltp: cover inotify unmount semantics |
| `d100a9941c3995bf17b8baca40aade0c567f908c` | `c703b876349aab40e8dcab0f50ec1d44a6ddfd8a` | `f8b882702c158444dc4e02a1112ffd84b769b73d` | checkpoint static LTP frontier work |
| `09b4c70a86f30cc2505f720add89e21636f7b1d2` | `0287852d415984271531140037f7e0a1b855e0cc` | `becd8e37f77a6e9768f77b724ab1542f2a6d374c` | checkpoint static LTP frontier work |
| `a16438b5702716cfa36eca0fbb1b5a46c0f933cc` | `6a361304bb8c01d02b7472178f24267cbc052f45` | `4886f759fe7d85b7c51a22b63f1b02d9852db908` | fix unaligned mmap backend relocation |
| `9f7232bb5769282021b3438842b77727deb2486d` | `6e7cfbe5650672e5759803fa09d52246d277d188` | `d5a6fe76ee8563da18324190c38df9ad31bc8eda` | ltp: promote cgroup smoke cases |
| `a8752a46c116f41cb77a77d4667b35ed832f0466` | `ff2769587e6fe89854dc3d70ec3103317de41fda` | `d67522932c28bf3403c89d2b841d7847e3dcfa0d` | ltp: promote key and module smoke cases |
| `8c0f5d57ae6fea5d1d4e60ef441a881ed927f342` | `f80c59455183bc0e50d74797454d676d5a744e05` | `47efe89b4a829947fa139e4eb7cb2e11683d76a7` | ltp: promote ipc message queue cases |
| `b8664e646f2d38defc3a81d9a45c9b1c36c7f69a` | `e2bb261b8455ae6c4ce72194ad13f61aabe4a25c` | `da6857eda6680c96fa64953b511f5f7b8eacc4da` | ltp: promote aio syscall cases |
| `c77af1a02e921a0d7a3b20e72da27fe63fdb4f8c` | `aff40fe60500779c9a0a7be3d0a04ce26cb929e2` | `ca287c5a925c098bc53a7b9361819b830b48231f` | ltp: fix acct and rlimit regressions |
| `d380161784501a47358b9f1ccaa06df68798ce8c` | `9035a54f4e19051cb2a9ec497b36d07e8ef5f8af` | `614c6af946265f256b9df23eff50dcb8c28e3abb` | ltp: promote process ptrace candidates |
| `cdcba3e291be79056f4f9dbef4e3ac1b954d8159` | `ad9a5485a70bf81ef80b62ba1968d5cddf168b5a` | `2ac2d4c45082047396d0b39e5534961a37ae7876` | ltp: fix SysV semaphore SETVAL and wait cleanup |
| `6cff9b55a8b7272551e19fab45c087b11f6fa203` | `9cd87054ca84a7a23a7c94a1208a4c0f343cfdc1` | `fc5e59645921367b612fd03a418aa7d30027ff6d` | ltp: promote SysV semaphore cases |
| `b3069f790162854ca695aa28d9f5616d7f6185e9` | `6c86a9bd6c3fefa5f293dddba77734223696ec2c` | `ae5a8b49d50b7774d15593cf463b11ac72f5b56b` | ltp: promote SysV IPC ftok cases |
| `e872d168f2a2b4b54f2e993b399f736d9dbd873e` | `2b279c335cfd363389003395a8a465e9cbf19fcd` | `ae2c6c5cbffefaa2a938b044aec8e1198229ed66` | ltp: promote SysV and mqueue timed cases |
| `e2d292ee8b61173290d91c0b2f74131379189fc9` | `87c4e840e9d2b092d6fc699d17a9a2f5a2740347` | `d376faa120f2107b30c1c366963791f5075d5343` | net: admit packet socket compatibility surface |
| `ce9a34b8e983f8b93985e5d5a8d40cd808b3e6e5` | `84080c646a4b9d053b05e13d38c8cdf03b546ac5` | `766e7d94991419fc92020d806188911888e9cd06` | ltp: promote TCP TLS ULP socket cases |
| `5d406bd40f292171cd00d43c78b3880a2b2f4164` | `f6a1e46d26d62ac9cb5794710c6ad1a7393d3e05` | `edd489e0efb319714a3a8774fca07a27e9e46547` | docs: tighten LTP campaign ledger guidance |
| `56619904f12557267391ab9464ce4a1a7c12f5e4` | `b99900fb5631826ea9fc256851aac972394d782d` | `53befb273fbde5008080664ff9fbdba7203aee2a` | ltp: promote mmap memory smoke cases |
| `769ec75e11247921727d20fa65fb5725d32a169d` | `4459d6e9e4e72a19947f979f353990726f077557` | `8e338195339961da9c373916ecad66074b9eec9b` | ltp: promote ioctl and pidfd smoke cases |
| `9e6c3ae1dc03b0b9a7cb4b91d3f44c1acb938a48` | `50c5aef9503709b32b06c255000acccb397f7504` | `281b557ab0a1ca8ca87d588f35e4a3b4663f237f` | ltp: promote memory and shm smoke cases |
| `714a59dfc608bcac9ec6743cceb1cc302a748c03` | `6776d1cc03df263dcb75759a805643c626b3f12c` | `6c32df207d39b3ccc448fd64c1f288f05f93a2d7` | ltp: promote pipe and semaphore smoke cases |
| `c7799593f93429d187663f59f7966607a1d4f1ac` | `3bf3be73d6706c90b0fe2ddb9240c5fe8de01659` | `5b7a29844b04d8dbe5afcad31895194e4c22463b` | ltp: promote scheduler script smoke cases |
| `c24c8c376614ec102f39c911bf5ffec5f29cf2bb` | `e5120cff9f761de7714304c094d9dcc5d5c49c46` | `c6b265b7f409b8597a42bc6a8b24ae9d939b1745` | task: guard active scope read ownership |
| `8dc7298070603164c165ed4f9425d4ea8c4c6204` | `8875c463eaaf3e15c123b9c8d5bf0a6680216eee` | `2c0a72110eb85e969cde99bf7f58606895e66fbe` | lab: tighten timeout log parsing |
| `c79a769db5a0e6da15189abaf3c3a337f6990af9` | `e55c238bb3dd9b8d8e075be76288d062e358130f` | `efe9914023ce756b16b92adadb884809ab3b1cb0` | ltp: promote add_key CVE cases |
| `9edad1c42f3b2103034165d615fcf20ed73bee1e` | `d0e84491ac591eeec9df60d5f281244f66e1f61b` | `8e52ba61ab767aa77bb1cc4d31518dbedaa6f9b6` | ltp: promote qmm mmap smoke case |
| `908e70966343a5df782a1f2732b04c1dfd96a4f9` | `c2121bf1eb3e4037b4592bf5f05f317d188c3950` | `c649cd458a45375c87cbaca2f387b36b522b95aa` | ltp: promote timerfd settime race case |
| `d3ae1a5dc575ad95fcf270dc1a5113a4077396e0` | `e85809d8e17595c27ccbe9788144c384c2b80b44` | `9f7155e55964665e553d234862894907da7c6098` | tmpfs: bound default capacity |
| `31279e9f09585da25fe22254872525e1cc504d03` | `e097164bf3dac64277c799da8ec03a31b7b8585f` | `ffeb2533ae15965bb3acec85377ec15325cfdeae` | ltp: promote sysv shm multiattach cases |
| `17a1f21f812e15941607d9d7b3d637c27a4e63ff` | `b72a56882351aab5c9f6cfedda5cd18f56e2a383` | `f21079d18bb6de162546c862eaa5b62328c3cc83` | Promote SysV semaphore LTP tail |
| `9d4a2b0571b780920b7cd8cfc80f94dca91fb6d2` | `9cdd75525e28f23ae9e87943ecd9c81b37af240e` | `8702c70d09accbe5af8169c5166a42f517164a9a` | ltp: promote sysctl command smoke |
| `8e143563192fd5ca53e483fc969c7444f9e48c24` | `ecfa65928cc0734e62d2c34ff89e7023ff4c8570` | `57c640658b2c3322666fe93b0b0d89302159603d` | tmpfs: expand var tmp ltp scratch capacity |
| `44e6d364838990f46e6dd66e9bdc80b3731f8277` | `3192911bcefbf3fd040751842ff75c3ea9a34d51` | `bfa1d6b77bceee3a5f6f0f6fb449a6aed2119911` | ltp: promote pthread scheduler smoke cases |
| `c72e3907014a2332cb73b2e3ffae9d0d313e8f5c` | `7c315572d4c1dea55e42e04d4b78ee00cf878258` | `58d24f6b2c27b795ca4771794eadf0d7e633cc42` | ltp: promote pthread tree scheduler case |
| `ca0e0223a1fc30580550422864efbf2f110245bf` | `33e57e6d39347b2a496a8356001c7e91c5938d36` | `2dab0d6c7e1614364f640d472dbd904fcd88544c` | Promote IPv6 libc LTP cases |
| `a2d309e29849c4585c4e14f9a7f812b9dd305783` | `ad3ab8336f1fa41277a49369990186d5ea53a985` | `fe050a2053a9a000810565f185ed3f9b6c493008` | Prepare oscomp submission checkpoint |
| `eb7b229ffe736e306b46de8bb23fc8d57c9bd645` | `01c8b3643f1bbf930a5867d4ec3dcf53678a37d1` | `94371725e643f35f31c8a3b14f3d031b19aa386d` | Prioritize early evaluator coverage |
| `d28ce85140bb56b3199e34bc5cb06b22ec911e5d` | `e7e36244f56e5f700f592133a28838e18b61c005` | `400571eace5e0a548a5b9c73584d8ce56853067f` | fix(oscomp): keep full plan and stabilize iozone |
| `49f0469ea27d20c17071935a1054d145e1a1679e` | `774f1d87790017be30cfd108074ba8e5f5a44081` | `8c0e0091960aa3b7455c8c0afee05dd451e443b0` | Align OSComp evaluator output markers |
| `f1959410664e18b7fb2d4b157eb3bc44d5863bcf` | `d904dc5b5e8b459e3b40f5b17b353b2e8a8959f6` | `eb7cd12c4d3d320720c504cf1d06c1bcb41120bd` | Harden syscall error paths |
| `bd7cee83db8febb9ba1f933d7a4ab2c2b04954c8` | `b28cfc92d273128b607650d7aec1a4e146fa69e3` | `e0a7cc1b4c9e7c44e02a6bb955e36fe5ab22e506` | Align OSComp runtime packaging |
| `ea424461fb267fdde26d80b3e1b7c20b34e5c3ff` | `323dc22658700ae99ae75869c31d66e5b4a47008` | `73c25bf422fec71f2cae6ca85c908612103c8d5e` | Harden kernel behavior for evaluator workloads |
| `54730b5d60b4f76832a651d033141f8ada32ea1b` | `64712152c0bc75c5ec72f6fd513157f09bd86a22` | `bc95e23d2c5e43c977efb3134bb537cd33db71d8` | Fix COW teardown deadlock under LTP |
| `65f48af5d0ca61775853a04636aa9bab77a0e1ca` | `9ecaea2b282584cab8b36122cbe03a4d292288ac` | `cef91ac2214f808ab91b8cfadf7bef4ed5ece924` | Avoid LTP timeout multiplier overflow |
| `6351841b2cc0d1b227342a855a7b48cd40bcd08e` | `fbe23b2f65d585134c428d5017f916ed47e65a03` | `94d4cdf685a3d58eafe0fe635e26610db460fa40` | Tune LTP list for evaluator runtime |
| `8fc0248a4d66b2253b6fab473db85ce59a482ea8` | `7a17903a4c799adeb50383299945099b609f2e3d` | `dda97fc30b3b538cf9447015d352035fc5b3b608` | Deduplicate LTP shell link test |
| `5b4ae565f57996286639fb94314ef6ab37701327` | `37fe10ce982816a877b2c1a9ffc72f2916e7e69f` | `c416a055dcd8d7269a1f74b49f2c2b3b5e5a45b7` | Use upstream fork06 workload duration |
| `d5b55cf1ac3071dc9a12fadccfac5ab910e6bed3` | `a05af6648dd10bd3d3360331e48d22331cf6b277` | `6ff2fdb4b57036733442769a2151ab9b0a526a1b` | Restore LTP case marker format and add LTP time watchdog |
| `032de9180848dc698c067f94cbc81f9507723b0d` | `d20cda28de72b706559ad5909863d06d4570f67c` | `d23097c63db15acac41a688d1c5886d799018baf` | Reorder eval plan to favor high-value groups for both libcs |
| `53990dc65d9e191ef47c2eb26bf2990b97f7ae3a` | `306ff16dfc4e36a9f0e7828c67af574964399390` | `ada78517cbf8af176ff7bde0a4b2061b980e4b13` | Kernel perf: larger caches/buffers, drop waste in sleep and stack recycle |
| `20112b4cf751bc7045ddba0d5f22599f3ae40634` | `1dd774cfc7d2b4cde64a1bb1a51a2740a0ac6844` | `542fb9d11aeb966114e1116a0dc523a20ef2bdbc` | Kernel perf: page-cache readahead and lazy user-copy fault-in |
| `551bc2cdc225a515b98d18944072ffc588adff4e` | `8b93c7df00dce9f90670ac9c0051012fd7666c8a` | `6b105e6abf51a598732f04e4c4fe11f3559dcda9` | Kernel perf: fast-path trivial getter syscalls |
| `d8d7c75e0f77d05d49c034036b27a74609b34d19` | `6085a2d019f39548588055a1bafcc8634a1b2612` | `b94f0d1ec1332f5762f69ec8682d9b1677bba7c7` | Kernel perf: skip full-page pre-zero for file-backed page faults |
| `0bccaa9844e53185232ae0fb022fd47418c1658f` | `f187f0bfc75d7de1b40ecea693c62f2f82373c3d` | `3e86e41dfeee9b4d4820180b1a4babdac4276d69` | Kernel perf: zero pages with a u64 store loop, not write_bytes |
| `e3c4028fc6c789f44bc2dd6521960af6f52a93f6` | `67af54d200315e67057d345305a1e5e48b7826b7` | `a4ec274c73454baa02978519ecb17295aa369af4` | Kernel perf: skip TLB flush on freshly-mapped leaf, u64-zero PT pages |
| `19d25390595f71c000582362184f2bd8afa91a5b` | `33d9950fe817ad9e47e0e1e43d2d7b3d59e17915` | `9658ff5447cb4c6dc252c25520a9f3060337cefc` | Kernel perf: skip userfaultfd scan on page faults when no uffd exists |
| `61c078015ab015e73fa088d8dfd8671fddd4b509` | `20e830f87c2b4e2603fcfb87ae06f8b84e6b56d9` | `9de9909bb95dcf7d800063b98ab534f0dc95be2c` | Kernel perf: skip RLIMIT_CPU check on every fault/syscall when no CPU limit set |
| `4b781b97cfe40cd7d1b6110052d493e63651d8b1` | `8d569dc42668e67f4a11a98232b1a8d0f99355dc` | `7ac6884b5590709d305194b739987879de843660` | Kernel perf: bigger readahead, deeper net queues, smaller fault_around, clock_gettime fast-path |
| `4a3409ddbc058988fc933d3c418fe8a7599dd3f6` | `68860992262e3e993bc7527ecbb00a3845b3dc46` | `f6e9ff8f2e512ab95fb6021428d2d52a0a306de0` | Kernel perf: revert TCP socket buffer to 256 KiB (fix smoltcp seq underflow panic) |
| `f63e620a51f6b04e4218fc79e73a6e75195a382d` | `58b2b953b70c4832547f9ff575eebea9d985f7dc` | `32a1e435d0e552312becc694bbb85a82cf352d6c` | OSComp: restore LA and prioritize score coverage |
| `780731a4c70d285cea0842ec3e2b48cf274c842b` | `35dc879477493d7c559dc94bf91f305c4eabd076` | `5641ef3f650b08496f684c9c7a975e57972742d8` | Kernel perf: improve loopback, scheduler, fs, and pipe paths |
| `1eceeee099e7641495836f187c2f97a449e79e59` | `34276afb96d34ff610ff672db14d867b958ee490` | `667169b70580f8afef54416253e368108eb1d0a2` | Fix smoltcp TCP window and timestamp handling |
| `e7bbc333e45c2ef8c6d03b2370308a5f18ab0bcd` | `f3358fcf7f65b7dd2d85b6cab208686512687381` | `f933fba385500bbca9017f05efd25115196ce217` | Bound LTP case runtime and cleanup temp state |
| `73f4361e13610b6a91348ddcd1866bd41d461ea1` | `c487b5eb183b4fe572879a6fddf7e8fa8250e684` | `c2db0614467c43ee6c080dcaeb9955b70c0b9a8c` | Bound exited task reclaim in syscall paths |
| `dc01f80628eada1b948f04496b088bda99ea0523` | `c16d8be35587f97e8c8c36e3cd2cf1952df2cfa0` | `e1b0efa24b9b006c29a95513f04342f918885445` | Avoid recursive cleanup after LTP cases |
| `0fbbff4b6ed6421b4e3aaa325ef7d0c13c7dc843` | `1f2a2f050cf9f081a1a61485d650633e65a01fe6` | `d383b8562dc9b35ff69aaf5750e1d88a43e4f422` | Align LTP lab timeout env names |
| `66b2c43cb88962eaae72c422919daceae991c3d0` | `1f6f0e8e811417b2990fbaeb483f5811fbf5448c` | `e57b967a64a910b74760f7790fcbd4d8f8ca6070` | Use uptime for LTP deadline timing |
| `e2ba74588a26eb5be87a57cfb5f5d2a0a691f2e8` | `ee536259b06b21dc103c51d580080f4086a0ec39` | `6a11fdb69dbc4ae65ee05daffa8cdf8c92b65988` | Qualify evaluator group markers by libc |
| `37e534b6b8a0482abf4b4ec82769843a8778a6a4` | `20e23b3c1d9bb471628f24046a18307e5771f419` | `3f4b44b2ebbc8e9e1e1604011979e8676d009b88` | Harden evaluator runner group handling |
| `2c348a8c3f4f7cbe1ecc0d222f07d2d26313a4b4` | `6f1402642a750193bfe8236417f98cdc0ca2481b` | `3fbfd0e530de92ab0ca922be6d1ab7defa190edc` | Tune evaluator test budgets |
| `bb81f45f8d60580b734e1ea5a058e6043ffd692d` | `6e6f9212945cc2f83c9803c5d7d70ca9109b2b26` | `83f75a494ad4b922a0bcb47dce932738dac73752` | Increase LTP group budgets |
| `2d14082a88c4cd9c0580ace14258f2f9a5629bb1` | `00deeab59e8758959d29525f4428949ad2e7c1e0` | `d9577e717e0d2b8c855d22b707e4b2e34f2dc88f` | Restore balanced LTP budgets |
| `e5209189d87bfdaaeca0db0d426caae4673f566a` | `c3ad3e25ee7b2211ae6386f7ba8a017003853681` | `99dd17c5fc2a40572523b7c5d6036aec0158831d` | Reorder LTP cases by timing density |
| `fe4ec848e79c25a24ac6317825b9819604e72b83` | `cc90e715a16bcd39bc85ee86f0b67bfcef8d7ef2` | `e82fbafcb783b19a339b6736261f8a74e9d06e42` | Raise default LTP group budget |
| `8a847ebbc718a76fa4852718642858c7fd9719b9` | `83c9565cf7de8f20d7db635b9735ca62da15d2a6` | `7b3a45f73833b84b4968d072dfc6898a15421783` | Prioritize benchmark groups before LTP |
| `c3d09b3c6b70ff75fa73d065ef851bb26dd03741` | `1dd6d25b3552c3f554a6f0cdd09b80d65bd1d4fc` | `2a7453616a05003afd9de461f03d95aaa6a20299` | Bound lmbench calibration before LTP |
| `5769b9f893c08f006505bf665b0159f43e66f271` | `6bd9428a2d34d0a3660e69ab24189b54d91db6f6` | `ade48821383dcac074f20f321bc9301a72611991` | Cache tmpfs pages and run iozone on tmpfs |
| `cea9e8b51cca0fc621e6589661491be5764f0e68` | `d8b592ccac1e318dcd92528797f07d8f2f33c199` | `7eedca37da93b89981b8bbc9724e8f1ca1f99f54` | Raise tmpfs page pool with memory guard |
| `130355019a06318bab932b272f0a0e46acb82663` | `e747db5b30ff81f599ab64eac58c47d329d24655` | `5548184c8f46c3538aef77dc3158e0c5b6669341` | Revert "Raise tmpfs page pool with memory guard" |
| `aef4298f8f56b2a772c21e7086ddbe4f36203930` | `a824809ef14743a2cef2e7a0949432da16a0986e` | `c63a5abd6118d7f2d4d63866dcffc45e7223e99f` | Revert "Cache tmpfs pages and run iozone on tmpfs" |
| `4508110997d16d7d60c7dfb924a74a7f8676d1d6` | `39bfa2012346de3458d8dcb75af947e08be72ad8` | `a2c01378c5eebecbce420910534a273da6e54163` | Prioritize iozone tasks with round-robin scheduling |
| `a309d05cfeb6c29bb669645fa097a1f63e33c437` | `27838e583456e6195c72da84a40dd39bed7d6e48` | `492c8362d4e1b2370d0b142a8cbe04315961aa9a` | Tune LTP default time budgets |
| `6d136321cd97da4aaf3736b4e92aa2bc47a800a3` | `2584d1f004e6029cfa90f4b1212abc2c15e01f96` | `4fe198e69c9839a91fd990ab59635e13ac293805` | Tune LTP ordering and benchmark budgets |
| `acd39ded7a432ee9a1f6df1af9270d489f2bdaa6` | `557d148f7f0d08322a56aba7f9ad75e37afcfa16` | `925a2b238c7036f30ea75e9273b923f122ab0304` | Bound cyclictest group execution |
| `7dba97890dab3d72e2875397cdd194e06aba9687` | `6a08658f3a379712c367b345756087d36d3a6623` | `48ffb4e185c5719f59a0250f2ff491c4214b7644` | Add OSComp 2026 documents |
| `8578ae7cde0aa323be9b647f2662ba76ec406410` | `0aa574bd1274aa622ef65f3bc34a7c18431864e9` | `a81d965f8d9500b8c9b361a6cab240e5a2034371` | Ignore local agent and editor state |
| `d041b49f66395f93bba93f14eb4851ba20cd6239` | `3e381ea3759cad4ca4f0005f4257a6a838c42f49` | `d4945058dae734beefccf276caf9916c57295fa5` | Move OSComp slides into docs |
| `690f0cf44a1b2a626b3ab46aaaefa7539f3550cf` | `18fbec696c6c8bd8b18b1e5ba7340f9bfc9112e2` | `4cc59ef56977ad912dc49b92b1e8e7e09aec2a3d` | Document license and build contract |
| `2853c979e0db13c5af9fa5691c5242146b0be91b` | `a00e1097586be02c2e9a6814f5406cf7d074f216` | `13f08bcf2b4b0bc3c59b00a9ff746d604509db23` | Add interactive boot targets |
| `f63c4db42a83c8a9caf2e1036eb4fad130e1ebd5` | `aa3ca12bf0f9d5760c43421a5bdcfaa464f8e746` | `9b2c597b80b98984ec007f99baf8f524e373fefa` | Clarify source and document licenses |
| `d1d106e9dfbc077f5ce7fc6f54dfa5a563958f14` | `f0bce9f67a481aaf8fbbdcf3f7f4aacca6c0b534` | `b0605566f32587195eb57e72f3a2901c7623b7b6` | Document dev-shell command workflow |
| `fa4b4ec06538e620fafba5ee5a2815d2a843aeef` | `0ed2c6432ee54efe9508a5096c530344e441f824` | `6a0b61bd50c238fe4be3d96604d752604e89d8e1` | Stabilize LTP replay tail |
| `9ebbbf0ed283c72972ed4b35f35068c8ae657c45` | `b4869c8fa6938d600e6c0819388150ba5a9fe839` | `369f7606280d111d73f7c198389e9a341210c5c2` | docs: fix OSComp report filename |
