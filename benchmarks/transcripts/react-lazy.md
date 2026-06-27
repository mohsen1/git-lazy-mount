# react — git lazy-mount

`[+6s]` I'll search for where `useState` resolves its initial state using `sgrep`.

`[+7s]` **🔧 Bash**

```
sgrep "mountState" 2>&1 | head -60
```

`[+7s]` **🔧 Bash**

```
sgrep "useState.*initialState" 2>&1 | head -40
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
fixtures/dom/src/components/fixtures/fragment-refs/EventDispatchCase.js:19:  const [clickedState, setClickedState] = useState({...initialState});
packages/react-reconciler/src/ReactInternalTypes.js:400:  useState<S>(initialState: (() => S) | S): [S, Dispatch<BasicStateAction<S>>],
packages/react-refresh/src/__tests__/ReactFreshIntegration-test.js:1064:            return React.useState(initialState);
packages/react-refresh/src/__tests__/ReactFreshIntegration-test.js:1080:            return React.useState(initialState);
packages/react-refresh/src/__tests__/ReactFreshIntegration-test.js:1097:     …[truncated]
```

`[+42s]` Now let me read the `mountStateImpl` function to see exactly where initial state is resolved.

`[+43s]` **🔧 Read**

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

`[+48s]` I've found the exact location. The initial state is resolved at line 1898–1901: if `initialState` is a function (lazy initializer), it's called there; either way, it's stored at line 1912. Let me add a clarifying comment.

`[+50s]` **🔧 Edit**

```
/work/lazy/packages/react-reconciler/src/ReactFiberHooks.js
```

_output:_

```
The file /work/lazy/packages/react-reconciler/src/ReactFiberHooks.js has been updated successfully. (file state is current in your context — no need to Read it back)
```

`[+52s]` Now create the branch and push.

`[+53s]` **🔧 Bash**

```
git checkout -b glm-bench-lazy && git add packages/react-reconciler/src/ReactFiberHooks.js && git commit -m "glm-bench: note where the answer lives" && git push -u origin glm-bench-lazy
```

_output:_

```
Switched to a new branch 'glm-bench-lazy'
[glm-bench-lazy 8d89e15c] glm-bench: note where the answer lives
 1 file changed, 2 insertions(+)
remote: 
remote: Create a pull request for 'glm-bench-lazy' on GitHub by visiting:        
remote:      https://github.com/mohsen1/react/pull/new/glm-bench-lazy        
remote: 
To https://github.com/mohsen1/react
 * [new branch]        glm-bench-lazy -> glm-bench-lazy
branch 'glm-bench-lazy' set up to track 'origin/glm-bench-lazy'.
```

`[+74s]` ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1898` — `mountStateImpl` resolves the initial state: if it's a function (lazy initializer), it's called once at line 1901; the result is stored into `hook.memoizedState` and `hook.baseState` at line 1914.


## Result `[+74s]` 

```
ANSWER: `packages/react-reconciler/src/ReactFiberHooks.js:1898` — `mountStateImpl` resolves the initial state: if it's a function (lazy initializer), it's called once at line 1901; the result is stored into `hook.memoizedState` and `hook.baseState` at line 1914.
```

_tool calls: 5 · duration: 72318 ms · cost: $0.1382154_
