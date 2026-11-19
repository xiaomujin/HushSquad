<script lang="ts">
  import type { Channel } from '../lib/ipc';
  import { backend } from '../lib/backend';
  import { app, FLAG_CONTAINER } from '../lib/state.svelte';

  // 新建 / 重命名 / 人数上限 / 删除确认共用一个壳。
  //
  // 四个动作的流程一模一样（填 → 提交 → 等服务器快照 → 关掉），差别只在「发哪条命令」
  // 和「什么算成功」。拆成四个组件会把这段流程抄四遍，改一次要改四处。

  let {
    mode,
    parentId = 0,
    parentName = '',
    channel = null,
    childCount = 0,
    memberCount = 0,
    onclose,
  }: {
    mode: 'create' | 'rename' | 'limit' | 'delete';
    parentId?: number;
    parentName?: string;
    channel?: Channel | null;
    childCount?: number;
    memberCount?: number;
    onclose: () => void;
  } = $props();

  const MAX_NAME = 32;
  const DEFAULT_MAX_USERS = 32;

  /**
   * 输入框的初始值只在**打开那一刻**算一次。
   *
   * 打开之后不再跟着 props 走：用户正在改的名字不该被新来的频道快照覆盖掉。
   * 放进函数里也是这个意思——这些 props 不是响应式依赖。
   */
  function initialName(): string {
    return mode === 'rename' ? (channel?.name ?? '') : '';
  }

  function initialLimit(): string {
    return mode === 'limit' ? String(channel?.max_users ?? 0) : '0';
  }

  let name = $state(initialName());
  let asContainer = $state(false);
  let limit = $state(initialLimit());

  let pending = $state(false);
  let failure = $state('');
  let timedOut = $state(false);
  /** 提交前的频道 id 快照：新建要看「多出来一个」，光比名字会撞上同名的旧频道。 */
  let before = $state<number[]>([]);
  /** 提交那一刻的提示序号。之后序号变大，才算这次操作的失败。 */
  let noticeBase = 0;

  let timer: ReturnType<typeof setTimeout> | undefined;
  let input = $state<HTMLInputElement | undefined>();

  const trimmed = $derived(name.trim());
  const nameError = $derived(
    mode !== 'create' && mode !== 'rename'
      ? ''
      : trimmed.length === 0
        ? '名字不能为空'
        : trimmed.length > MAX_NAME
          ? `名字最多 ${MAX_NAME} 个字`
          : '',
  );
  const limitValue = $derived(Number.parseInt(limit, 10));
  const limitError = $derived(
    mode === 'limit' && (Number.isNaN(limitValue) || limitValue < 0) ? '请填 0 或正整数' : '',
  );
  /** 改成和现在一样：服务器不会给任何变化，等下去只会超时，所以先拦住。 */
  const unchanged = $derived(
    mode === 'rename'
      ? trimmed === (channel?.name ?? '')
      : mode === 'limit'
        ? limitValue === (channel?.max_users ?? 0)
        : false,
  );
  const blockedByMembers = $derived(mode === 'delete' && memberCount > 0);
  const canSubmit = $derived(
    !pending && !blockedByMembers && !nameError && !limitError && !unchanged,
  );

  const title = $derived(
    mode === 'create'
      ? parentId === 0
        ? '新建频道'
        : `在「${parentName}」下新建子频道`
      : mode === 'rename'
        ? '重命名频道'
        : mode === 'limit'
          ? '人数上限'
          : '删除频道',
  );

  const goLabel = $derived(pending ? '处理中…' : mode === 'delete' ? '删除' : '确定');

  /**
   * 期望的变化出现了吗。
   *
   * 成功与否**只看服务器发来的频道表**，不看命令有没有返回：命令返回只说明请求送出去
   * 了，真正的结果在下一份快照里。
   */
  function arrived(channels: Channel[]): boolean {
    if (mode === 'create') {
      return channels.some(
        (c) => c.parent === parentId && c.name === trimmed && !before.includes(c.id),
      );
    }
    if (!channel) return false;
    switch (mode) {
      case 'rename':
        return channels.find((c) => c.id === channel.id)?.name === trimmed;
      case 'limit':
        return channels.find((c) => c.id === channel.id)?.max_users === limitValue;
      case 'delete':
        return !channels.some((c) => c.id === channel.id);
    }
  }

  function stopTimer(): void {
    clearTimeout(timer);
    timer = undefined;
  }

  function submit(): void {
    if (!canSubmit) return;
    if (mode !== 'create' && !channel) return;
    failure = '';
    timedOut = false;
    before = app.channels.map((c) => c.id);
    noticeBase = app.noticeSeq;
    pending = true;
    // 3 秒兜底：服务器既不广播也不拒绝时，按钮不能永远转下去。
    stopTimer();
    timer = setTimeout(() => {
      if (!pending) return;
      pending = false;
      timedOut = true;
    }, 3000);
    void run().catch((error) => {
      // 命令本身被拒（权限不足、参数不合法）时 invoke 会 reject，也要看得见。
      stopTimer();
      pending = false;
      failure = String(error);
    });
  }

  async function run(): Promise<void> {
    switch (mode) {
      case 'create':
        await backend.createChannel(parentId, trimmed, asContainer ? FLAG_CONTAINER : 0, 0);
        break;
      case 'rename':
        await backend.updateChannel(channel?.id ?? 0, { name: trimmed });
        break;
      case 'limit':
        await backend.updateChannel(channel?.id ?? 0, { maxUsers: limitValue });
        break;
      case 'delete':
        await backend.deleteChannel(channel?.id ?? 0);
        break;
    }
  }

  // 成功：对话框关掉（由父组件卸载它，所以这里只需要调 onclose）。
  $effect(() => {
    if (!pending) return;
    if (arrived(app.channels)) {
      stopTimer();
      onclose();
    }
  });

  // 失败：服务器拒绝会走 onNotice → app.notice。把它抓进对话框用红字显示，
  // 但**不关**对话框——用户改个名字就能重试。
  $effect(() => {
    if (!pending) return;
    if (app.noticeSeq === noticeBase) return;
    stopTimer();
    pending = false;
    failure = app.notice ?? '操作被服务器拒绝';
    // 原因已经在这个对话框里了，底部那条浮条收回去，别把同一句话显示两遍。
    app.dismissNotice();
  });

  // 自动聚焦：一开就能打字。`autofocus` 属性会招 a11y 警告，所以用代码聚焦。
  $effect(() => {
    const element = input;
    if (!element) return;
    element.focus();
    if (mode === 'rename') element.select();
  });

  // 卸载时清掉兜底计时器（它闭包着组件的状态，留着没意义）。
  $effect(() => () => stopTimer());

  function onkey(event: KeyboardEvent): void {
    if (event.key !== 'Escape') return;
    event.stopPropagation();
    onclose();
  }
</script>

<svelte:window onkeydown={onkey} />

<!-- 遮罩用 `<button>` 是为了键盘可达，与右键菜单的 `.ctx-scrim` 同一个理由。 -->
<button class="dlg-scrim" aria-label="取消" onclick={onclose}></button>

<div class="dlg" role="dialog" aria-modal="true" aria-label={title}>
  <div class="dlg-t">{title}</div>

  {#if mode === 'create' || mode === 'rename'}
    <label class="dlg-f">
      名字
      <input
        class="dlg-inp"
        bind:this={input}
        bind:value={name}
        spellcheck="false"
        placeholder="比如 大厅"
        onkeydown={(event) => {
          if (event.key === 'Enter') submit();
        }}
      />
    </label>
    {#if nameError}
      <div class="dlg-err">{nameError}</div>
    {:else if unchanged}
      <div class="dlg-hint">名字没有变化。</div>
    {/if}
    {#if mode === 'create'}
      <label class="dlg-chk">
        <input type="checkbox" bind:checked={asContainer} />
        建成分组（不可加入，只用来聚合频道）
      </label>
    {/if}
  {:else if mode === 'limit'}
    <label class="dlg-f">
      人数上限
      <input
        class="dlg-inp"
        type="number"
        min="0"
        bind:this={input}
        value={limit}
        oninput={(event) => (limit = event.currentTarget.value)}
        onkeydown={(event) => {
          if (event.key === 'Enter') submit();
        }}
      />
    </label>
    <div class="dlg-hint">0 = 用服务器默认（{DEFAULT_MAX_USERS} 人）。</div>
    {#if limitError}
      <div class="dlg-err">{limitError}</div>
    {:else if unchanged}
      <div class="dlg-hint">人数上限没有变化。</div>
    {/if}
  {:else}
    <div class="dlg-body">
      确定删除 <b>{channel?.name ?? ''}</b>？
    </div>
    {#if childCount > 0}
      <div class="dlg-hint">会连同 {childCount} 个子频道一起删除。</div>
    {/if}
    {#if blockedByMembers}
      <div class="dlg-err">频道里还有 {memberCount} 个人，先把这 {memberCount} 个人移到别的频道。</div>
    {/if}
    <div class="dlg-hint">删除之后没法撤销。</div>
  {/if}

  {#if failure}
    <div class="dlg-err">{failure}</div>
  {/if}
  {#if timedOut}
    <div class="dlg-err">服务器没有响应</div>
  {/if}

  <div class="dlg-acts">
    <button class="btn" type="button" onclick={onclose}>取消</button>
    <button
      class="btn-primary dlg-go"
      class:danger={mode === 'delete'}
      type="button"
      disabled={!canSubmit}
      onclick={submit}
    >
      {goLabel}
    </button>
  </div>
</div>
