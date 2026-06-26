# react — full

`[+4s]` I'll search for where `useState` resolves its initial state.

**🔧 Bash**

```
sgrep "mountState" 2>/dev/null | head -40
```

**🔧 Bash**

```
sgrep "useState" 2>/dev/null | head -30
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

`[+38s]` Now let me look at the `mountStateImpl` function:

**🔧 Read**

```
/work/full/packages/react-reconciler/src/ReactFiberHooks.js
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

`[+44s]` Found it. The initial state is resolved at line 1898–1912: if `initialState` is a function (lazy initializer), it's called; the result (or the value directly) is stored in `hook.memoizedState`. Now I'll add a clarifying comment and push it.

**🔧 Edit**

```
/work/full/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
The file /work/full/packages/react-reconciler/src/ReactFiberHooks.js has been updated successfully. (file state is current in your context — no need to Read it back)
```

**🔧 Bash**

```
git checkout -b glm-bench-full && git add packages/react-reconciler/src/ReactFiberHooks.js && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-full
```

_output:_

```
Switched to a new branch 'glm-bench-full'
[glm-bench-full c900383921] glm-bench: note where the answer lives
 1 file changed, 2 insertions(+)
remote: 
remote: Create a pull request for 'glm-bench-full' on GitHub by visiting:        
remote:      https://github.com/mohsen1/react/pull/new/glm-bench-full        
remote: 
To https://github.com/mohsen1/react
 * [new branch]            glm-bench-full -> glm-bench-full
branch 'glm-bench-full' set up to track 'origin/glm-bench-full'.
```

`[+55s]` ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves initial state by calling it if it's a function (lazy initializer), then stores the result in `hook.memoizedState`.


## Result `[+56s]` 

```
ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1896` — `mountStateImpl` resolves initial state by calling it if it's a function (lazy initializer), then stores the result in `hook.memoizedState`.
```

_tool calls: 5 · duration: 54730 ms · cost: $0.13760685_
