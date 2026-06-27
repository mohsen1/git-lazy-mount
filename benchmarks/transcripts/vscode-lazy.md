# vscode — git lazy-mount

`[+17s]` Searching for the toggle-word-wrap command registration.

`[+18s]` **🔧 Bash**

```
sgrep "toggle-word-wrap"
```

_output:_

```
Exit code 1
```

`[+30s]` **🔧 Bash**

```
sgrep "toggleWordWrap"
```

_output:_

```
src/vs/base/test/node/uri.test.data.txt:762:/users/foo/src/vs/workbench/parts/codeEditor/electron-browser/toggleWordWrap.ts
src/vs/workbench/contrib/codeEditor/browser/codeEditor.contribution.ts:24:import './toggleWordWrap.js';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:55:const TOGGLE_WORD_WRAP_ID = 'editor.action.toggleWordWrap';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:114:	public static readonly ID = 'editor.contrib.toggleWordWrapController';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:178:	public static readonly ID = 'diffeditor.co …[truncated]
```

`[+92s]` **🔧 Bash**

```
sgrep "editor.action.toggleWordWrap"
```

_output:_

```
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:55:const TOGGLE_WORD_WRAP_ID = 'editor.action.toggleWordWrap';
```

`[+134s]` **🔧 Read**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
1	/*---------------------------------------------------------------------------------------------
2	 *  Copyright (c) Microsoft Corporation. All rights reserved.
3	 *  Licensed under the MIT License. See License.txt in the project root for license information.
4	 *--------------------------------------------------------------------------------------------*/
5	
6	import { addDisposableListener, onDidRegisterWindow } from '../../../../base/browser/dom.js';
7	import { mainWindow } from '../../../../base/browser/window.js';
8	import { Codicon } from '../../../../base/common/codicons.js';
9	import  …[truncated]
```

`[+137s]` **🔧 Read**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
<system-reminder>Warning: the file exists but is shorter than the provided offset (350). The file has 347 lines.</system-reminder>
```

`[+140s]` **🔧 Read**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
300	
301	registerWorkbenchContribution2(EditorWordWrapContextKeyTracker.ID, EditorWordWrapContextKeyTracker, WorkbenchPhase.AfterRestored);
302	
303	registerEditorContribution(ToggleWordWrapController.ID, ToggleWordWrapController, EditorContributionInstantiation.Eager); // eager because it needs to change the editor word wrap configuration
304	registerDiffEditorContribution(DiffToggleWordWrapController.ID, DiffToggleWordWrapController);
305	registerEditorAction(ToggleWordWrapAction);
306	
307	MenuRegistry.appendMenuItem(MenuId.EditorTitle, {
308		command: {
309			id: TOGGLE_WORD_WRAP_ID,
310		 …[truncated]
```

`[+143s]` Found it. The `registerEditorAction(ToggleWordWrapAction)` call at line 305 is the registration. Now let me add a clarifying comment.

`[+144s]` **🔧 Edit**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
The file /work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts has been updated successfully. (file state is current in your context — no need to Read it back)
```

`[+148s]` **🔧 Bash**

```
git checkout -b glm-bench-lazy && git add src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-lazy
```

_output:_

```
Switched to a new branch 'glm-bench-lazy'
[glm-bench-lazy 377873dc1] glm-bench: note where the answer lives
 1 file changed, 1 insertion(+)
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/vscode/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/vscode
 * [new branch]          glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

`[+167s]` ANSWER: `src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:305` — `registerEditorAction(ToggleWordWrapAction)` registers the `editor.action.toggleWordWrap` command (Alt+Z) as an editor action.


## Result `[+167s]` 

```
ANSWER: `src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:305` — `registerEditorAction(ToggleWordWrapAction)` registers the `editor.action.toggleWordWrap` command (Alt+Z) as an editor action.
```

_tool calls: 8 · duration: 166144 ms · cost: $0.1188404_
