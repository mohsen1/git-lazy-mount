# react — git lazy-mount

I'll search for where `useState` resolves its initial state using `sgrep`.

**🔧 Bash**

```
sgrep "mountState" 2>/dev/null | head -50
```

_output:_

```
(Bash completed with no output)
```

**🔧 Bash**

```
sgrep "useState" 2>/dev/null | head -30
```

_output:_

```
(Bash completed with no output)
```

**🔧 Bash**

```
which sgrep && sgrep --help 2>&1 | head -20
```

_output:_

```
/home/ubuntu/bin/sgrep
/home/ubuntu/bin/sgrep: line 2: /usr/local/bin/sgrep-bin: No such file or directory
```

**🔧 Bash**

```
ls /work/lazy/packages/react-reconciler/src/ 2>/dev/null | head -30
```

_output:_

```
ReactCapturedValue.js
ReactChildFiber.js
ReactCurrentFiber.js
ReactEventPriorities.js
ReactFiber.js
ReactFiberAct.js
ReactFiberActivityComponent.js
ReactFiberApplyGesture.js
ReactFiberAsyncAction.js
ReactFiberAsyncDispatcher.js
ReactFiberBeginWork.js
ReactFiberCacheComponent.js
ReactFiberCallUserSpace.js
ReactFiberClassComponent.js
ReactFiberClassUpdateQueue.js
ReactFiberCommitEffects.js
ReactFiberCommitHostEffects.js
ReactFiberCommitViewTransitions.js
ReactFiberCommitWork.js
ReactFiberCompleteWork.js
ReactFiberComponentStack.js
ReactFiberConcurrentUpdates.js
ReactFiberConfig.js
ReactFiberConf …[truncated]
```

**🔧 Bash**

```
ls /work/lazy/packages/react-reconciler/src/ 2>/dev/null | grep -i hook
```

_output:_

```
Permission to use Bash with command grep -i hook has been denied.
```

**🔧 Glob**

```
**/ReactFiberHooks*
```

_output:_

```
packages/react-reconciler/src/ReactFiberHooks.js
```

**🔧 Agent**

```
{"description": "Find mountState in ReactFiberHooks.js", "subagent_type": "Explore", "prompt": "Search the file /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js for the `mountState` function. I want to know: 1) what line number it's on, 2) how it handles the initial state argument (specifically, does it call initialState as a function if it's a function?). Show me the full function body …[truncated]
```

**🔧 Read**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
<system-reminder>[Truncated: PARTIAL view — showing lines 1-2089 of 5242 total (53299 tokens, cap 25000). Call Read with offset=2090 limit=2089 for the next page, or Grep to find a specific section. Do NOT answer from this page alone if the answer may be further in the file.]</system-reminder>

1	/**
2	 * Copyright (c) Meta Platforms, Inc. and affiliates.
3	 *
4	 * This source code is licensed under the MIT license found in the
5	 * LICENSE file in the root directory of this source tree.
6	 *
7	 * @flow
8	 */
9	
10	import type {
11	  ReactContext,
12	  StartTransitionOptions,
13	  Usable,
14	  …[truncated]
```

**🔧 Bash**

```
grep -n "^function mountState" /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
Permission to use Bash with command grep -n "^function mountState" /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js has been denied.
```

**🔧 Read**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
1924	function mountState<S>(
1925	  initialState: (() => S) | S,
1926	): [S, Dispatch<BasicStateAction<S>>] {
1927	  const hook = mountStateImpl(initialState);
1928	  const queue = hook.queue;
1929	  const dispatch: Dispatch<BasicStateAction<S>> = dispatchSetState.bind(
1930	    null,
1931	    currentlyRenderingFiber,
1932	    queue,
1933	  ) as any;
1934	  queue.dispatch = dispatch;
1935	  return [hook.memoizedState, dispatch];
1936	}
1937	
1938	function updateState<S>(
1939	  initialState: (() => S) | S,
1940	): [S, Dispatch<BasicStateAction<S>>] {
1941	  return updateReducer(basicStateReduc …[truncated]
```

**🔧 Read**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
1896	function mountStateImpl<S>(initialState: (() => S) | S): Hook {
1897	  const hook = mountWorkInProgressHook();
1898	  if (typeof initialState === 'function') {
1899	    const initialStateInitializer = initialState;
1900	    // $FlowFixMe[incompatible-use]: Flow doesn't like mixed types
1901	    initialState = initialStateInitializer();
1902	    if (shouldDoubleInvokeUserFnsInHooksDEV) {
1903	      setIsStrictModeForDevtools(true);
1904	      try {
1905	        // $FlowFixMe[incompatible-use]: Flow doesn't like mixed types
1906	        initialStateInitializer();
1907	      } finally {
1908 …[truncated]
```

_output:_

```
Perfect! I have all the information needed. Let me provide you with the findings:

## Search Results: `mountState` Function

**Location:** Line 1924 in `/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js`

### Answer to Your Questions:

1. **Line Number:** `mountState` starts at **line 1924**

2. **Initial State Handling:** Yes, `mountState` **does call initialState as a function if it's a function**. However, the actual function invocation logic is delegated to `mountStateImpl` (lines 1896-1922), which `mountState` calls on line 1927. The key logic is in `mountStateImpl` at lines 189 …[truncated]
```

Found it. Now I'll read the exact lines and add a clarifying comment.

**🔧 Read**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
1893	  }
1894	}
1895	
1896	function mountStateImpl<S>(initialState: (() => S) | S): Hook {
1897	  const hook = mountWorkInProgressHook();
1898	  if (typeof initialState === 'function') {
1899	    const initialStateInitializer = initialState;
1900	    // $FlowFixMe[incompatible-use]: Flow doesn't like mixed types
1901	    initialState = initialStateInitializer();
1902	    if (shouldDoubleInvokeUserFnsInHooksDEV) {
1903	      setIsStrictModeForDevtools(true);
1904	      try {
1905	        // $FlowFixMe[incompatible-use]: Flow doesn't like mixed types
1906	        initialStateInitializer();
1907	 …[truncated]
```

**🔧 Edit**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
The file /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js has been updated successfully. (file state is current in your context — no need to Read it back)
```

**🔧 Bash**

```
git checkout -b glm-bench-lazy && git add packages/react-reconciler/src/ReactFiberHooks.js && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-lazy
```

_output:_

```
Exit code 143
Command timed out after 2m 0s
Switched to a new branch 'glm-bench-lazy'
```

**🔧 Bash**

```
git status && git log --oneline -3
```

_output:_

```
On branch glm-bench-lazy
Changes to be committed:
  (use "git restore --staged <file>..." to unstage)
	modified:   packages/react-reconciler/src/ReactFiberHooks.js

Changes not staged for commit:
  (use "git add <file>..." to update what will be committed)
  (use "git restore <file>..." to discard changes in working directory)
	modified:   CLAUDE.md


It took 5.14 seconds to enumerate untracked files,
but the results were cached, and subsequent runs may be faster.
See 'git help status' for information on how to improve this.

52912a14 [DevTools] Add ignore-listed stack frame disclosure (#36828 …[truncated]
```

**🔧 Bash**

```
git commit -m "glm-bench: note where the answer lives"
```

_output:_

```
[glm-bench-lazy 4f08a854] glm-bench: note where the answer lives
 1 file changed, 3 insertions(+)
```

**🔧 Bash**

```
git push -u origin glm-bench-lazy 2>&1
```

_output:_

```
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/react/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/react
 * [new branch]        glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1899` — `mountStateImpl` calls the lazy initializer function (if `typeof initialState === 'function'`) and stores the result as `hook.memoizedState`/`hook.baseState`, resolving `useState`'s initial state on first render.


## Result

```
ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1899` — `mountStateImpl` calls the lazy initializer function (if `typeof initialState === 'function'`) and stores the result as `hook.memoizedState`/`hook.baseState`, resolving `useState`'s initial state on first render.
```

_tool calls: 17 · duration: 206959 ms · cost: $0.2141795_
