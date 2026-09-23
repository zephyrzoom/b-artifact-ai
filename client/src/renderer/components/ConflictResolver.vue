<template>
  <el-card shadow="never" data-testid="conflict-resolver">
    <template #header>
      <div class="flex">
        <span class="mono">{{ info.path }}</span>
        <el-tag size="small" :type="info.reason === 'both-modified' ? 'warning' : 'danger'" effect="plain">
          {{ REASON_LABEL[info.reason] }}
        </el-tag>
        <el-tag v-if="!info.mergeable" size="small" type="info" effect="plain">
          不可文本合并 · 只能二选一
        </el-tag>
        <div class="spacer" />
        <span class="muted" style="font-size: 12px">服务端版本：r{{ info.theirs_rev }}</span>
      </div>
    </template>

    <el-alert
      :title="hint"
      :type="info.mergeable ? 'info' : 'warning'"
      :closable="false"
      show-icon
      style="margin-bottom: 12px"
      data-testid="conflict-hint"
    />

    <div class="panes">
      <div class="pane">
        <div class="pane-title">
          基线（冲突前的共同祖先）
          <el-tag size="small" effect="plain">{{ kindLabel(sides.base.kind) }}</el-tag>
        </div>
        <pre class="content mono" data-testid="conflict-base">{{ sideText(sides.base) }}</pre>
      </div>
      <div class="pane">
        <div class="pane-title">
          我的版本
          <el-tag size="small" type="warning" effect="plain">{{ kindLabel(sides.mine.kind) }}</el-tag>
        </div>
        <pre class="content mono" data-testid="conflict-mine">{{ sideText(sides.mine) }}</pre>
      </div>
      <div class="pane">
        <div class="pane-title">
          服务端版本
          <el-tag size="small" type="success" effect="plain">{{ kindLabel(sides.theirs.kind) }}</el-tag>
        </div>
        <pre class="content mono" data-testid="conflict-theirs">{{ sideText(sides.theirs) }}</pre>
      </div>
    </div>

    <div v-if="info.mergeable" class="merge">
      <div class="pane-title" style="margin: 12px 0 6px">合并结果（可编辑后保存）</div>
      <el-input
        v-model="merged"
        type="textarea"
        :rows="4"
        data-testid="conflict-merge-input"
        placeholder="在这里写出最终内容，保存后会覆盖工作文件"
      />
    </div>

    <div class="flex" style="margin-top: 14px">
      <el-button
        type="primary"
        :loading="busy"
        data-testid="conflict-take-mine"
        @click="$emit('resolve', 'mine')"
      >
        保留我的版本
      </el-button>
      <el-button
        :loading="busy"
        data-testid="conflict-take-theirs"
        @click="$emit('resolve', 'theirs')"
      >
        {{ info.reason === 'deleted-remotely' ? '接受服务端删除' : '使用服务端版本' }}
      </el-button>
      <el-button
        v-if="info.mergeable"
        type="success"
        :loading="busy"
        :disabled="merged.length === 0"
        data-testid="conflict-save-merged"
        @click="$emit('resolve', 'merged', merged)"
      >
        保存合并结果
      </el-button>
      <div class="spacer" />
      <span class="muted" style="font-size: 12px" data-testid="conflict-count">
        {{ solvedCount }} / {{ total }} 已解决
      </span>
    </div>
  </el-card>
</template>

<script setup lang="ts">
import { computed, ref, watch } from 'vue';

import type { ConflictInfo, ConflictReason, ConflictSides, ConflictResolution, SideContent, SideKind } from '@shared/dto';

const props = defineProps<{
  info: ConflictInfo;
  sides: ConflictSides;
  busy: boolean;
  /** 全局进度（第几个 / 共几个），由父组件给。 */
  solvedCount: number;
  total: number;
}>();

defineEmits<{
  (e: 'resolve', choice: ConflictResolution, content?: string): void;
}>();

const REASON_LABEL: Record<ConflictReason, string> = {
  'both-modified': '双方都改了',
  'deleted-remotely': '服务端已删除',
};

const KIND_LABEL: Record<SideKind, string> = {
  text: '文本',
  binary: '二进制',
  'too-large': '文件过大',
  missing: '不存在',
};

const merged = ref('');

// 切换冲突项时重置编辑框（不要把上一个文件的合并结果带过来）
watch(
  () => props.info.path,
  () => {
    merged.value = props.sides.mine.text ?? '';
  },
  { immediate: true },
);

const hint = computed(() => {
  if (props.info.reason === 'deleted-remotely') {
    return '服务端已删除该文件，而本地还有改动。保留会把它重新纳入版本控制（提交时记为新增）；接受则删除本地文件。';
  }
  if (!props.info.mergeable) {
    return '这三方不是纯文本（或文件过大），无法做文本合并——二进制资源只能整体二选一。';
  }
  return '三方都是文本，可以手工合并后保存；保存的内容会成为新的工作文件，提交前仍是"已修改"状态。';
});

function kindLabel(k: SideKind): string {
  return KIND_LABEL[k];
}

function sideText(s: SideContent): string {
  if (s.kind === 'text') return s.text ?? '';
  if (s.kind === 'missing') return '（不存在）';
  if (s.kind === 'binary') return `（二进制内容，${s.size} 字节，不可预览）`;
  return `（文件过大，${s.size} 字节，已跳过文本对比）`;
}
</script>

<style scoped>
.panes {
  display: grid;
  grid-template-columns: repeat(3, minmax(0, 1fr));
  gap: 10px;
}
.pane {
  border: 1px solid var(--ba-border);
  border-radius: 6px;
  overflow: hidden;
}
.pane-title {
  padding: 6px 10px;
  background: #fafafa;
  border-bottom: 1px solid var(--ba-border);
  font-size: 12px;
  display: flex;
  align-items: center;
  gap: 8px;
}
.content {
  margin: 0;
  padding: 10px;
  height: 200px;
  overflow: auto;
  white-space: pre-wrap;
  word-break: break-all;
  font-size: 12px;
  line-height: 1.6;
}
</style>
