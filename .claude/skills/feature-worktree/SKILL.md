---
name: feature-worktree
description: Before implementing a new feature, isolate it in its own git worktree (own folder + branch) AND its own Herdr pane so parallel sessions never share a checkout or a terminal. Use whenever the user asks to implement, build, add, or work on a new feature, view, panel, or other sizable change to either plugin. Skip for trivial edits or bug fixes on the current branch.
---

# Feature worktree

When the user asks for a new feature, give it **its own git worktree** (own folder + branch)
**and its own Herdr pane** before writing code — several agent sessions often run against
this repo at once, and a shared checkout means branch switches and half-finished edits land
under each other's feet. One feature = one worktree = one pane, named after the feature.

## Steps

1. **Already isolated? Just build.** If this session's cwd is already under
   `.claude/worktrees/`, you ARE the feature pane — implement the request right here, don't
   nest another worktree or spawn another pane. Skip to step 5.

2. **Not in Herdr? Fall back to in-place.** If `HERDR_ENV` is not `1`, there are no panes to
   spawn — enter the worktree in THIS session with the **EnterWorktree tool**
   (`name: <slug>`) and build here. Skip the pane steps.

3. **Derive a short kebab-case slug** from the request ("add a diff view to herdr-aa-git" →
   `git-diff-view`).

4. **Spawn the feature's pane, then hand it the task.** Do this from the current session;
   keep your own focus where it is (`--no-focus` everywhere).

   a. **Create the worktree FIRST — before the pane exists.** You are running Bash (Git Bash),
      so `&&` is fine here; it is only the PowerShell *panes* that reject it. If the repo has
      an `origin` remote, fetch and branch from `origin/main`; otherwise branch from `main`:
      ```bash
      git fetch origin && git worktree add .claude/worktrees/<slug> -b <slug> origin/main
      # no origin yet:
      git worktree add .claude/worktrees/<slug> -b <slug> main
      ```
      Order matters: doing this first lets the pane START inside the worktree (step c), which
      buys two things. (1) **herdr's recorded cwd for that pane is then honest** — if you
      instead `Set-Location` after the shell boots, herdr keeps reporting the shell's START
      dir (the main checkout) forever, so every feature pane claims to be somewhere it is not.
      (2) No shell chaining in the pane at all — launching Claude becomes a bare command.

   b. **Find which tab the pane goes in. Cap: 4 panes per tab** (past ~4 the splits get
      unusably small). Placement order — **fill existing tabs before making new ones**:
      1. **This tab has room** (< 4 panes) → put it here.
      2. **This tab is full** → use the FIRST OTHER AGENT TAB (in tab order) that has room.
         Do NOT mint a new tab just because this one is full — that strands one agent per
         tab and wastes the grid.
      3. **Every agent tab is full** → only then create a new tab.

      **Never place into one of the user's own tabs** (their lazygit/shell tab). A tab is
      only a candidate if EVERY pane in it is a claude agent — that is what keeps feature
      panes out of e.g. the `git` tab.

      ```bash
      python -c "
      import json,subprocess,os
      ws=os.environ['HERDR_WORKSPACE_ID']
      tabs=json.loads(subprocess.check_output(['herdr','tab','list','--workspace',ws]))['result']['tabs']
      panes=json.loads(subprocess.check_output(['herdr','pane','list','--workspace',ws]))['result']['panes']
      by={}
      for p in panes: by.setdefault(p['tab_id'],[]).append(p)
      cur=os.environ.get('HERDR_TAB_ID')
      for t in sorted(tabs,key=lambda t:t['number']):
          ps=by.get(t['tab_id'],[]); n=len(ps)
          agents = n>0 and all(p.get('agent')=='claude' for p in ps)
          state = 'CANDIDATE' if (agents and n<4) else ('full' if agents else 'SKIP(not agent tab)')
          print('%-6s %-22s panes=%d %-19s %s' % (t['tab_id'],(t.get('label') or '')[:22],n,state,'<-- current' if t['tab_id']==cur else ''))
      "
      ```

   c. **Make the pane — the tab must end up a 2x2 GRID. Max 2 panes in each direction:
      NEVER 3 stacked on top of each other, never 3 side by side.** Split the LARGEST pane
      in the target tab, not necessarily your own — splitting your own every time shreds it
      (112 cols → 56 → a 17-row sliver), so the full-width/full-height pane is the one that
      should give up space. Growth sequence for a tab:
      - **1 pane** → split it RIGHT → two columns.
      - **2 panes (side by side)** → split the larger one DOWN.
      - **3 panes** → split the remaining full-width/full-height one to complete the 2x2.
      - **4 panes** → full. Go place it elsewhere (step b).

      Read the rects, pick the biggest, and choose the direction that does NOT put a 3rd
      pane in the same row or column. **Always pass `--cwd <absolute path to the worktree>`**
      so the pane starts inside it (see step a for why):
      ```bash
      herdr pane layout --pane <any pane id in the target tab>   # compare x/y/w/h, pick the biggest
      herdr pane split <biggest-pane-id> --direction down --cwd "<repo>/.claude/worktrees/<slug>" --no-focus
      ```
      If instead every agent tab was full, create a new tab (it takes `--cwd` too):
      ```bash
      herdr tab create --workspace "$HERDR_WORKSPACE_ID" --label "<slug>" --cwd "<repo>/.claude/worktrees/<slug>" --no-focus
      ```
      Read the new pane id from the JSON response — `result.pane.pane_id` for a split,
      `result.root_pane.pane_id` for a new tab. Call it `<pane>`. Never construct the id by
      hand. Sanity-check the returned `cwd` is the worktree.

   d. Label the pane, then **relabel the TAB to all its pane names concatenated**:
      ```bash
      herdr pane rename <pane> "<slug>"
      ```
      The pane `label` renders on the pane border, and on this machine the agents sidebar is
      configured to show it too (see the sidebar note in another repo's feature-worktree
      skill for the config details). Rebuild the tab label from the tab's panes in grid order
      (top-left → bottom-right) every time you add a pane:
      ```bash
      python -c "
      import json,subprocess,os,sys
      tab=sys.argv[1]
      ws=os.environ['HERDR_WORKSPACE_ID']
      panes=[p for p in json.loads(subprocess.check_output(['herdr','pane','list','--workspace',ws]))['result']['panes'] if p['tab_id']==tab]
      lay=json.loads(subprocess.check_output(['herdr','pane','layout','--pane',panes[0]['pane_id']]))['result']['layout']
      order={p['pane_id']:(p['rect']['y'],p['rect']['x']) for p in lay['panes']}
      panes.sort(key=lambda p: order.get(p['pane_id'],(0,0)))
      label=' | '.join((p.get('label') or '?') for p in panes)
      subprocess.check_output(['herdr','tab','rename',tab,label]); print(label)
      " <target-tab-id>
      ```
      Note Claude Code also overwrites the pane's *terminal title* with an auto-generated
      topic summary — that is a third string. Pane label / terminal title / tab label are all
      different things; don't confuse them.

   e. **Launch Claude.** The pane already starts in the worktree, so this is a bare command —
      no `cd`, no chaining. Use **`--dangerously-skip-permissions`** so the delegated agent
      doesn't stall on tool-permission or MCP-trust prompts:
      ```bash
      herdr pane run <pane> 'claude --dangerously-skip-permissions'
      herdr wait agent-status <pane> --status idle --timeout 120000
      ```
      Then confirm the pane shows a clean prompt (`herdr pane read <pane> --source recent`)
      before handing off — text sent while Claude is still booting is silently lost.
      **If you ever do need a multi-command line in a pane: panes run Windows PowerShell 5.1,
      so chain with `;` + `if ($?)`, NOT `&&`** (which is a parser error).

   f. Hand off the feature, and tell that agent to close its own pane when it's fully done:
      ```bash
      herdr pane run <pane> "<the feature request, verbatim>. Build it in this worktree, commit on the <slug> branch, and push to origin if one exists. When the feature is merged into main and this worktree has been removed, close this pane with: herdr pane close \"\$HERDR_PANE_ID\""
      ```

5. **Implement** inside the worktree, commit on the `<slug>` branch. Work happens inside the
   affected plugin directory (`plugins/herdr-aa-filetree` or `plugins/herdr-aa-git`) — build, test,
   and lint there before calling it done:
   ```bash
   cargo build --release && cargo test && cargo clippy -- -D warnings
   ```
   Stay in this session/pane the whole time.

## Merge & teardown (closes the pane)

The pane lives exactly as long as the feature does. Whoever merges the feature is responsible
for closing its pane:

**Order matters** — pane first, then orphans, then the worktree. Anything still sitting *in*
the worktree holds a Windows lock on it and `git worktree remove` fails with "Permission
denied" / "Device or resource busy".

1. **Check the agent is actually finished and its work is safe.** `herdr pane get <pane>` →
   `agent_status` must be `idle`/`done` (never merge or close a `working` pane out from under
   itself), and the worktree must have no uncommitted changes:
   ```bash
   git -C .claude/worktrees/<slug> status --porcelain
   ```
   Non-empty = uncommitted work; deal with it first.
2. Merge the `<slug>` branch into `main` (from the main checkout — a worktree can't check out
   `main` while the primary checkout holds it). **Verify the MERGED tree, not just the branch**
   — each agent only tested its own branch in isolation, so re-run build + tests + clippy in
   the affected plugin dir(s) after merging. Then delete the branch (and its remote copy if
   pushed).
3. **Close the feature's pane.** If you ARE that pane's agent, close yourself:
   `herdr pane close "$HERDR_PANE_ID"`. If you're cleaning up from another pane, find it by its
   slug label (`herdr pane list --workspace "$HERDR_WORKSPACE_ID"`) and `herdr pane close <id>`.
   Closing a pane ends the Claude session inside it — its branch is already merged, so no code
   is lost, but any unsaved conversation context is. If that pane still has an *unmerged*
   commit, merge or preserve it before closing.
4. **Kill the pane's ORPHANS — closing the pane does NOT kill them.** Agents leave behind
   stray processes (test binaries under `cargo test`, the plugin TUI launched for a manual
   check, `bash`/`tail`/`grep` helpers), and each holds the worktree dir open.
   **The Windows Restart Manager CANNOT see these** — a directory-CWD lock is invisible to it,
   so you must read each process's PEB for its cwd. This finds them; kill only PIDs whose cwd
   is under `.claude/worktrees/` so the user's own editor/terminal is never touched:
   ```powershell
   Add-Type -TypeDefinition @'
   using System; using System.Runtime.InteropServices; using System.Text;
   public static class Peb {
     [DllImport("ntdll.dll")] static extern int NtQueryInformationProcess(IntPtr h, int cls, ref PBI pbi, int len, out int ret);
     [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr OpenProcess(int a, bool i, int pid);
     [DllImport("kernel32.dll", SetLastError=true)] static extern bool CloseHandle(IntPtr h);
     [DllImport("kernel32.dll", SetLastError=true)] static extern bool ReadProcessMemory(IntPtr h, IntPtr b, byte[] buf, int size, out int read);
     [StructLayout(LayoutKind.Sequential)] struct PBI { public IntPtr R1; public IntPtr PebBaseAddress; public IntPtr R2a; public IntPtr R2b; public IntPtr Pid; public IntPtr R3; }
     static IntPtr Ptr(IntPtr h, IntPtr a){ byte[] b=new byte[8]; int r; if(!ReadProcessMemory(h,a,b,8,out r)) return IntPtr.Zero; return (IntPtr)BitConverter.ToInt64(b,0);}
     public static string GetCwd(int pid){ IntPtr h=OpenProcess(0x0400|0x0010,false,pid); if(h==IntPtr.Zero) return null;
       try{ var p=new PBI(); int ret; if(NtQueryInformationProcess(h,0,ref p,Marshal.SizeOf(p),out ret)!=0) return null;
         IntPtr pp=Ptr(h,(IntPtr)((long)p.PebBaseAddress+0x20)); if(pp==IntPtr.Zero) return null;
         byte[] L=new byte[2]; int r; if(!ReadProcessMemory(h,(IntPtr)((long)pp+0x38),L,2,out r)) return null;
         ushort len=BitConverter.ToUInt16(L,0); if(len==0||len>1024) return null;
         IntPtr buf=Ptr(h,(IntPtr)((long)pp+0x40)); if(buf==IntPtr.Zero) return null;
         byte[] s=new byte[len]; if(!ReadProcessMemory(h,buf,s,len,out r)) return null;
         return Encoding.Unicode.GetString(s);
       } finally { CloseHandle(h);} }
   }
   '@ -Language CSharp
   Get-CimInstance Win32_Process | ForEach-Object {
     $cwd=$null; try{ $cwd=[Peb]::GetCwd([int]$_.ProcessId) }catch{}
     if($cwd -and $cwd -like "*\.claude\worktrees\*"){ "$($_.ProcessId) $($_.Name) -> $cwd"; Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
   }
   ```
5. **Remove the worktree**: `git worktree remove --force .claude/worktrees/<slug>` then
   `git worktree prune`. If the directory still lingers on disk (git deregisters it but Windows
   refuses the delete), `rm -rf` it — an orphan you missed in step 4 is still holding it.
6. **Relabel that tab** — it is named after its panes, so drop the one you just closed. Re-run
   the concatenation snippet from step 4d against the tab. (A tab emptied of all panes
   auto-closes, so there may be no tab left to rename.)

## Notes

- **Rearranging existing panes**: `herdr pane move <pane> --tab <same tab>` is a **NO-OP** —
  it reports success and the layout does not change. To re-split a pane that is already in
  the tab (e.g. to fix 3 stacked panes into a 2x2), move it OUT and back:
  ```bash
  herdr pane move <pane> --new-tab --label tmp --no-focus
  herdr pane move <pane> --tab <target tab> --split right --target-pane <anchor> --no-focus
  ```
  Same-workspace moves **preserve the public pane id** (so an agent's `$HERDR_PANE_ID`
  self-close stays valid) and a tab that is emptied by a move **auto-closes**.
- The main checkout stays free for the user / other sessions; the feature pane owns the
  worktree until merge + teardown above.
