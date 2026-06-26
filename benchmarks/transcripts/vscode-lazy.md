# vscode — git lazy-mount

`[+67s]` I'll search for the toggle-word-wrap command registration using sgrep.

**🔧 Bash**

```
sgrep "toggle-word-wrap" 2>&1 | head -60
```

_output:_

```
(Bash completed with no output)
```

**🔧 Bash**

```
sgrep "toggleWordWrap" 2>&1 | head -60
```

_output:_

```
src/vs/base/test/node/uri.test.data.txt:762:/users/foo/src/vs/workbench/parts/codeEditor/electron-browser/toggleWordWrap.ts
src/vs/workbench/contrib/codeEditor/browser/codeEditor.contribution.ts:24:import './toggleWordWrap.js';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:55:const TOGGLE_WORD_WRAP_ID = 'editor.action.toggleWordWrap';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:114:	public static readonly ID = 'editor.contrib.toggleWordWrapController';
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:178:	public static readonly ID = 'diffeditor.co …[truncated]
```

**🔧 Bash**

```
sgrep "editor.action.toggleWordWrap" 2>&1 | head -60
```

_output:_

```
src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:55:const TOGGLE_WORD_WRAP_ID = 'editor.action.toggleWordWrap';
```

**🔧 Read**

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

**🔧 Read**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
290				const wrappingInfo = this._activeEditor.getOption(EditorOption.wrappingInfo);
291				this._setValues(true, wrappingInfo.wrappingColumn !== -1);
292			}
293		}
294	
295		private _setValues(canToggleWordWrap: boolean, isWordWrap: boolean): void {
296			this._canToggleWordWrap.set(canToggleWordWrap);
297			this._editorWordWrap.set(isWordWrap);
298		}
299	}
300	
301	registerWorkbenchContribution2(EditorWordWrapContextKeyTracker.ID, EditorWordWrapContextKeyTracker, WorkbenchPhase.AfterRestored);
302	
303	registerEditorContribution(ToggleWordWrapController.ID, ToggleWordWrapController, Editor …[truncated]
```

`[+215s]` Now I have the exact location. The command is registered at line 305 via `registerEditorAction(ToggleWordWrapAction)`. Let me add a clarifying comment there.

**🔧 Edit**

```
/work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts
```

_output:_

```
The file /work/lazy/src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts has been updated successfully. (file state is current in your context — no need to Read it back)
```

**🔧 Bash**

```
git checkout -b glm-bench-lazy && git add src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-lazy
```

_output:_

```
Switched to a new branch 'glm-bench-lazy'
[glm-bench-lazy f4a87dde3] glm-bench: note where the answer lives
 1 file changed, 1 insertion(+)
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/vscode/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/vscode
 * [new branch]          glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

`[+245s]` ANSWER: `src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:305` — `registerEditorAction(ToggleWordWrapAction)` registers the `editor.action.toggleWordWrap` command (bound to Alt+Z) as an editor action.


## Result `[+246s]` 

```
ANSWER: `src/vs/workbench/contrib/codeEditor/browser/toggleWordWrap.ts:305` — `registerEditorAction(ToggleWordWrapAction)` registers the `editor.action.toggleWordWrap` command (bound to Alt+Z) as an editor action.
```

_tool calls: 7 · duration: 243749 ms · cost: $0.11019844999999999_
