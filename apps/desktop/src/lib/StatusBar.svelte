<script lang="ts">
  import type { DaemonStatus } from "./api";

  let { status }: { status: DaemonStatus | null } = $props();
</script>

<div class="status-bar">
  <span class="dot" class:reachable={status?.reachable}></span>
  {#if status?.reachable}
    <span>本机 daemon · {status.page_count ?? "?"} 页</span>
  {:else}
    <span>本机 daemon 未运行</span>
  {/if}
</div>

<style>
  .status-bar {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.35rem 1rem;
    font-size: 0.8rem;
    color: #666;
  }

  .dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #c33;
    display: inline-block;
  }

  .dot.reachable {
    background: #3a3;
  }
</style>
