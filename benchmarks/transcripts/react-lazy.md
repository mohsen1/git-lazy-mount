# react — lazy

`[+19s]` I'll search for where `useState` resolves its initial state using `sgrep`.

**🔧 Bash**

```
sgrep "mountState" 2>/dev/null | head -60
```

**🔧 Bash**

```
sgrep "useState" 2>/dev/null | head -40
```

_output:_

```
packages/react-reconciler/src/ReactFiberHooks.js:1896:function mountStateImpl<S>(initialState: (() => S) | S): Hook {
packages/react-reconciler/src/ReactFiberHooks.js:1924:function mountState<S>(
packages/react-reconciler/src/ReactFiberHooks.js:1927:  const hook = mountStateImpl(initialState);
packages/react-reconciler/src/ReactFiberHooks.js:2401:  const pendingStateHook = mountStateImpl(false as Thenable<boolean> | boolean);
packages/react-reconciler/src/ReactFiberHooks.js:3297:  // essentially an inlined version of mountState.
packages/react-reconciler/src/ReactFiberHooks.js:3405:  const sta …[truncated]
```

_output:_

```
CHANGELOG.md:237:* Require initial argument to `useRef`: The initial argument is now required to match `useState`, `createContext` etc  
CHANGELOG.md:274:  * Restore double invoking `useState()` and `useReducer()` initializer functions in *StrictMode* ([\#28248](https://github.com/facebook/react/pull/28248) by [@eps1lon](https://github.com/eps1lon))  
CHANGELOG.md:852:* Add support for editing `useState` state from DevTools. ([@bvaughn](https://github.com/bvaughn) in [#14906](https://github.com/facebook/react/pull/14906))
CHANGELOG.md:957:* Bail out of rendering on identical values for `useSta …[truncated]
```

`[+49s]` Found it. Now let me look at `mountStateImpl` in `ReactFiberHooks.js` around line 1896.

**🔧 Read**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
1890	  const root = enqueueConcurrentRenderForLane(fiber, SyncLane);
1891	  if (root !== null) {
1892	    scheduleUpdateOnFiber(root, fiber, SyncLane);
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
1903	      setIsStrictModeForDe …[truncated]
```

`[+57s]` The key logic is clear. In `mountStateImpl` at line 1898-1912: if the initial state is a function (lazy initializer), it's called immediately; the result is stored in `hook.memoizedState`. I'll add a clarifying comment there.

**🔧 Edit**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
The file /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js has been updated successfully. (file state is current in your context — no need to Read it back)
```

`[+63s]` Now create the branch and push.

**🔧 Bash**

```
git checkout -b glm-bench-lazy && git add packages/react-reconciler/src/ReactFiberHooks.js && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-lazy
```

_output:_

```
Switched to a new branch 'glm-bench-lazy'
[glm-bench-lazy 3fa9107b] glm-bench: note where the answer lives
 1 file changed, 2 insertions(+)
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/react/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/react
 * [new branch]        glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

`[+185s]` ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves the initial state: if it's a function (lazy initializer) it is called immediately on line 1901, and the result is stored in `hook.memoizedState = hook.baseState` on line 1912.


## Result `[+185s]` 

```
ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves the initial state: if it's a function (lazy initializer) it is called immediately on line 1901, and the result is stored in `hook.memoizedState = hook.baseState` on line 1912.
```

_tool calls: 5 · duration: 168925 ms · cost: $0.09079469999999999_
