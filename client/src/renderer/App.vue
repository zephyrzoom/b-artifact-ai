<template>
  <LoginView v-if="!store.loggedIn" />

  <el-container v-else style="height: 100%">
    <el-aside width="196px" class="aside">
      <div class="brand">
        <span class="logo">b</span>
        <div>
          <div class="brand-name">b-artifact</div>
          <div class="brand-sub">资产客户端</div>
        </div>
      </div>
      <el-menu :default-active="tab" class="menu" data-testid="menu" @select="onSelect">
        <el-menu-item index="workspace" data-testid="menu-workspace">
          <el-icon><Files /></el-icon><span>工作副本</span>
        </el-menu-item>
        <el-menu-item index="repos" data-testid="menu-repos">
          <el-icon><FolderOpened /></el-icon><span>仓库</span>
        </el-menu-item>
        <el-menu-item index="history" data-testid="menu-history">
          <el-icon><Clock /></el-icon><span>历史</span>
        </el-menu-item>
        <el-menu-item index="conflicts" data-testid="menu-conflicts">
          <el-icon><WarnTriangleFilled /></el-icon>
          <span>冲突</span>
          <el-badge
            v-if="store.conflicts.length"
            :value="store.conflicts.length"
            type="danger"
            style="margin-left: 8px"
          />
        </el-menu-item>
        <el-menu-item index="settings" data-testid="menu-settings">
          <el-icon><Setting /></el-icon><span>设置</span>
        </el-menu-item>
      </el-menu>

      <div class="aside-foot">
        <div class="muted" style="font-size: 12px">
          <div data-testid="user">{{ store.auth?.username }}</div>
          <div class="mono" style="word-break: break-all">{{ store.auth?.server }}</div>
        </div>
        <el-button text size="small" data-testid="logout" @click="store.logout()">退出登录</el-button>
      </div>
    </el-aside>

    <el-container>
      <el-header class="header">
        <div class="flex" style="min-width: 0">
          <template v-if="store.wc">
            <el-tag size="small" type="success" effect="plain" data-testid="head-repo">
              {{ store.wc.repo }}
            </el-tag>
            <el-tag size="small" data-testid="head-rev">r{{ store.wc.rev }}</el-tag>
            <span class="mono muted ellipsis" data-testid="head-root">{{ store.wc.root }}</span>
          </template>
          <span v-else class="muted" data-testid="head-no-wc">未打开工作副本</span>
        </div>
        <div class="spacer" />
        <el-tag
          v-if="store.conflicts.length"
          size="small"
          type="danger"
          effect="dark"
          data-testid="head-conflicts"
          @click="tab = 'conflicts'"
        >
          {{ store.conflicts.length }} 个冲突待解决
        </el-tag>
        <span class="muted" style="font-size: 12px" data-testid="head-summary">
          {{ summaryText(store.summary) }}
        </span>
        <el-tag v-if="store.busy" size="small" type="warning" effect="plain">处理中…</el-tag>
      </el-header>

      <el-main style="padding: 0; overflow: auto">
        <WorkspaceView v-if="tab === 'workspace'" @goto="tab = $event" />
        <ReposView v-else-if="tab === 'repos'" @goto="tab = $event" />
        <ConflictView v-else-if="tab === 'conflicts'" />
        <HistoryView v-else-if="tab === 'history'" />
        <SettingsView v-else />
      </el-main>
    </el-container>

    <el-alert
      v-if="store.flash"
      class="flash"
      :type="store.flash.kind === 'ok' ? 'success' : 'error'"
      :title="store.flash.text"
      :closable="true"
      data-testid="flash"
      show-icon
      @close="store.flash = null"
    />
  </el-container>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue';
import { Clock, Files, FolderOpened, Setting, WarnTriangleFilled } from '@element-plus/icons-vue';

import ConflictView from '@/views/ConflictView.vue';
import HistoryView from '@/views/HistoryView.vue';
import LoginView from '@/views/LoginView.vue';
import SettingsView from '@/views/SettingsView.vue';
import ReposView from '@/views/ReposView.vue';
import WorkspaceView from '@/views/WorkspaceView.vue';
import { useAppStore } from '@/stores/app';
import { summaryText } from '@/utils/status';

const store = useAppStore();
const tab = ref('workspace');

function onSelect(index: string): void {
  tab.value = index;
}

onMounted(() => {
  void store.init();
});
</script>

<style scoped>
.aside {
  background: #fff;
  border-right: 1px solid var(--ba-border);
  display: flex;
  flex-direction: column;
}
.brand {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 14px 16px;
}
.brand-name {
  font-weight: 600;
}
.brand-sub {
  font-size: 11px;
  color: var(--ba-muted);
}
.logo {
  width: 30px;
  height: 30px;
  border-radius: 8px;
  background: #2f6fed;
  color: #fff;
  display: grid;
  place-items: center;
  font-weight: 700;
}
.menu {
  border-right: none;
  flex: 1;
}
.aside-foot {
  padding: 12px 16px;
  border-top: 1px solid var(--ba-border);
}
.header {
  display: flex;
  align-items: center;
  gap: 8px;
  background: #fff;
  border-bottom: 1px solid var(--ba-border);
}
.ellipsis {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 420px;
}
.flash {
  position: fixed;
  right: 16px;
  bottom: 16px;
  width: 380px;
  z-index: 2000;
}
</style>
