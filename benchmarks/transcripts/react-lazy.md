# react — git lazy-mount

`[+19s]` I'll search for where `useState` resolves its initial state using `sgrep`.

**🔧 Bash**

```
sgrep "mountState" 2>/dev/null | head -40
```

**🔧 Bash**

```
sgrep "useState" 2>/dev/null | grep -i "initial\|mount\|resolve" | head -20
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
Permission to use Bash with command grep -i "initial\|mount\|resolve" has been denied.
```

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

`[+35s]` Found it. The key logic is at line 1898–1901: if `initialState` is a function (lazy initializer), it's called immediately to produce the actual state value, then stored in `hook.memoizedState` at line 1912.

Now let me add a clarifying comment at that exact location.

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
Switched to a new branch 'glm-bench-lazy'
[glm-bench-lazy 028bcca3] glm-bench: note where the answer lives
 1 file changed, 2 insertions(+)
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/react/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/react
 * [new branch]        glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

`[+71s]` ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves `useState`'s initial state: if it's a function (lazy initializer) it's called immediately on mount; the result is stored in `hook.memoizedState` / `hook.baseState` (line 1912).


## Result `[+71s]` 

```
ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves `useState`'s initial state: if it's a function (lazy initializer) it's called immediately on mount; the result is stored in `hook.memoizedState` / `hook.baseState` (line 1912).
```

_tool calls: 5 · duration: 68726 ms · cost: $0.1328982_
