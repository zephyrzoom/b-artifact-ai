<template>
  <div class="login-wrap">
    <el-card class="login-card">
      <div class="head">
        <span class="logo">b</span>
        <div>
          <h2>b-artifact</h2>
          <p class="muted">集中式二进制资产版本管理</p>
        </div>
      </div>

      <el-form label-width="72px" @submit.prevent>
        <el-form-item label="服务器">
          <el-input
            v-model="server"
            data-testid="login-server"
            placeholder="http://127.0.0.1:8080"
            class="mono"
          />
        </el-form-item>
        <el-form-item label="用户名">
          <el-input v-model="username" data-testid="login-username" placeholder="用户名" />
        </el-form-item>
        <el-form-item label="密码">
          <el-input
            v-model="password"
            data-testid="login-password"
            type="password"
            placeholder="密码"
            show-password
            @keyup.enter="submit"
          />
        </el-form-item>
        <el-form-item>
          <el-button
            type="primary"
            style="width: 100%"
            :loading="store.busy"
            data-testid="login-submit"
            @click="submit"
          >
            登录
          </el-button>
        </el-form-item>
      </el-form>

      <div v-if="store.config?.servers?.length" class="servers">
        <div class="muted" style="font-size: 12px; margin-bottom: 6px">最近服务器</div>
        <el-tag
          v-for="s in store.config.servers"
          :key="s"
          class="server-tag"
          size="small"
          effect="plain"
          :data-testid="`login-server-${s}`"
          @click="server = s"
        >
          {{ s }}
        </el-tag>
      </div>

      <el-alert
        v-if="store.flash?.kind === 'err'"
        :title="store.flash.text"
        type="error"
        :closable="false"
        show-icon
        style="margin-top: 12px"
        data-testid="login-error"
      />

      <p class="muted hint">
        首次登录：服务端 users 表为空时，第一个成功登录的用户自动成为系统管理员（方案 §9.1）。
      </p>
      <p v-if="store.info" class="muted hint mono" data-testid="login-appinfo">
        Electron {{ store.info.electron }} · Node {{ store.info.node }} · {{ store.info.platform }}
      </p>
    </el-card>
  </div>
</template>

<script setup lang="ts">
import { onMounted, ref } from 'vue';

import { useAppStore } from '@/stores/app';

const store = useAppStore();
const server = ref('http://127.0.0.1:8080');
const username = ref('');
const password = ref('');

onMounted(async () => {
  // 配置可能还没加载完（init 是异步的），这里等一次
  if (!store.config) {
    await new Promise((r) => setTimeout(r, 50));
  }
  const servers = store.config?.servers ?? [];
  if (servers.length > 0) server.value = servers[0]!;
});

async function submit(): Promise<void> {
  if (!server.value || !username.value || !password.value) {
    store.note('请填写服务器、用户名与密码', 'err');
    return;
  }
  await store.login(server.value, username.value, password.value);
}
</script>

<style scoped>
.login-wrap {
  height: 100%;
  display: grid;
  place-items: center;
  background: radial-gradient(1000px 500px at 50% -10%, #2f6fed22, transparent);
}
.login-card {
  width: 430px;
}
.head {
  display: flex;
  align-items: center;
  gap: 12px;
  margin-bottom: 16px;
}
.head h2 {
  margin: 0;
  font-size: 19px;
}
.head p {
  margin: 2px 0 0;
  font-size: 12px;
}
.logo {
  width: 38px;
  height: 38px;
  border-radius: 10px;
  background: #2f6fed;
  color: #fff;
  display: grid;
  place-items: center;
  font-weight: 700;
  font-size: 19px;
}
.servers {
  margin-top: 4px;
}
.server-tag {
  margin: 0 6px 6px 0;
  cursor: pointer;
}
.hint {
  font-size: 12px;
  margin: 12px 0 0;
  line-height: 1.6;
}
</style>
