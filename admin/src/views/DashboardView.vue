<template>
  <div class="page">
    <h3 class="page-title">概览</h3>
    <p class="page-sub">系统规模、存储收益与最近活动</p>

    <el-skeleton v-if="loading" :rows="6" animated />

    <template v-else-if="stats">
      <el-row :gutter="12">
        <el-col v-for="c in cards" :key="c.label" :span="3">
          <el-card shadow="never" class="stat">
            <div class="stat-label">{{ c.label }}</div>
            <div class="stat-value">{{ c.value }}</div>
            <div v-if="c.sub" class="stat-sub muted">{{ c.sub }}</div>
          </el-card>
        </el-col>
      </el-row>

      <el-row :gutter="12" style="margin-top: 12px">
        <el-col :span="14">
          <el-card shadow="never">
            <template #header><span>近 30 天提交趋势</span></template>
            <div ref="trendEl" style="height: 240px" />
          </el-card>
        </el-col>
        <el-col :span="10">
          <el-card shadow="never">
            <template #header><span>存储收益</span></template>
            <div ref="storeEl" style="height: 240px" />
            <div class="muted" style="font-size: 12px; margin-top: 6px">
              逻辑 {{ formatBytes(stats.storage.logical_bytes) }} → 唯一
              {{ formatBytes(stats.storage.unique_bytes) }} → 落盘
              {{ formatBytes(stats.storage.stored_bytes) }}
            </div>
          </el-card>
        </el-col>
      </el-row>

      <el-row :gutter="12" style="margin-top: 12px">
        <el-col :span="14">
          <el-card shadow="never">
            <template #header>
              <div class="flex">
                <span>最近活动</span>
                <div class="spacer" />
                <el-button text type="primary" size="small" @click="$router.push('/audit')">
                  查看全部
                </el-button>
              </div>
            </template>
            <el-table :data="stats.recent_activity" size="small" max-height="300">
              <el-table-column prop="ts" label="时间" width="140">
                <template #default="{ row }">{{ formatTime(row.ts) }}</template>
              </el-table-column>
              <el-table-column prop="username" label="用户" width="110" />
              <el-table-column prop="action" label="动作" width="130">
                <template #default="{ row }">{{ actionLabel(row.action) }}</template>
              </el-table-column>
              <el-table-column prop="target" label="对象" min-width="160" class-name="mono" />
            </el-table>
          </el-card>
        </el-col>
        <el-col :span="10">
          <el-card shadow="never">
            <template #header><span>仓库规模 Top</span></template>
            <el-table :data="stats.top_repos" size="small" max-height="300">
              <el-table-column prop="name" label="仓库" min-width="120">
                <template #default="{ row }">
                  <el-link type="primary" :underline="false" @click="goto(row.name)">
                    {{ row.name }}
                  </el-link>
                </template>
              </el-table-column>
              <el-table-column prop="head_rev" label="修订" width="70" align="right" />
              <el-table-column label="文件" width="70" align="right" prop="files" />
              <el-table-column label="体积" width="90" align="right">
                <template #default="{ row }">{{ formatBytes(row.bytes) }}</template>
              </el-table-column>
            </el-table>
          </el-card>
        </el-col>
      </el-row>
    </template>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref } from 'vue'
import { useRouter } from 'vue-router'
import { ElMessage } from 'element-plus'
import * as echarts from 'echarts'
import { adminApi } from '@/api'
import type { Stats } from '@/api/types'
import { ApiError } from '@/api/http'
import { formatBytes, formatPercent, formatTime, actionLabel } from '@/utils/format'

const router = useRouter()
const stats = ref<Stats | null>(null)
const loading = ref(true)
const trendEl = ref<HTMLElement>()
const storeEl = ref<HTMLElement>()
let trendChart: echarts.ECharts | null = null
let storeChart: echarts.ECharts | null = null

const cards = computed(() => {
  const s = stats.value
  if (!s) return []
  return [
    { label: '仓库', value: s.repos },
    { label: '用户', value: s.users, sub: `${s.users_active} 启用` },
    { label: '会话', value: s.sessions },
    { label: '文件', value: s.files },
    { label: '修订', value: s.revisions },
    { label: '锁', value: s.locks },
    {
      label: '去重率',
      value: formatPercent(s.storage.dedup_ratio, 0),
      sub: `${s.storage.blob_count} 个 blob`,
    },
    {
      label: '压缩率',
      value: formatPercent(s.storage.compress_ratio, 0),
      sub: `落盘 ${formatBytes(s.storage.stored_bytes)}`,
    },
  ]
})

function renderCharts() {
  if (!stats.value) return
  const dark = document.documentElement.classList.contains('dark')
  const textColor = dark ? '#c9d1d9' : '#4c566a'

  if (trendEl.value) {
    trendChart = trendChart ?? echarts.init(trendEl.value)
    const t = stats.value.commit_trend
    trendChart.setOption(
      {
        tooltip: { trigger: 'axis' },
        grid: { left: 44, right: 16, top: 24, bottom: 30 },
        xAxis: {
          type: 'category',
          data: t.map((x) => x.date.slice(5)),
          axisLabel: { color: textColor, fontSize: 11 },
          axisLine: { lineStyle: { color: dark ? '#3a4048' : '#dcdfe6' } },
        },
        yAxis: {
          type: 'value',
          axisLabel: { color: textColor, fontSize: 11 },
          splitLine: { lineStyle: { color: dark ? '#2b3038' : '#ebeef5' } },
        },
        series: [
          {
            name: '提交',
            type: 'bar',
            data: t.map((x) => x.commits),
            itemStyle: { color: '#2f6fed', borderRadius: [3, 3, 0, 0] },
          },
          {
            name: '文件变更',
            type: 'line',
            smooth: true,
            data: t.map((x) => x.files),
            itemStyle: { color: '#67c23a' },
          },
        ],
      },
      { notMerge: true },
    )
  }

  if (storeEl.value) {
    storeChart = storeChart ?? echarts.init(storeEl.value)
    const st = stats.value.storage
    storeChart.setOption(
      {
        tooltip: { trigger: 'item', formatter: '{b}: {c} ({d}%)' },
        legend: { bottom: 0, textStyle: { color: textColor, fontSize: 11 } },
        series: [
          {
            type: 'pie',
            radius: ['48%', '70%'],
            avoidLabelOverlap: false,
            label: { show: false },
            data: [
              { name: '去重节省', value: Math.max(0, st.logical_bytes - st.unique_bytes) },
              {
                name: '压缩节省',
                value: Math.max(0, st.unique_bytes - st.stored_bytes),
              },
              { name: '实际落盘', value: st.stored_bytes },
            ],
            color: ['#409eff', '#67c23a', '#e6a23c'],
          },
        ],
      },
      { notMerge: true },
    )
  }
}

function onResize() {
  trendChart?.resize()
  storeChart?.resize()
}

onMounted(async () => {
  try {
    stats.value = await adminApi.stats()
    await nextTick()
    renderCharts()
    window.addEventListener('resize', onResize)
  } catch (e) {
    ElMessage.error(e instanceof ApiError ? e.message : '加载概览失败')
  } finally {
    loading.value = false
  }
})

onBeforeUnmount(() => {
  window.removeEventListener('resize', onResize)
  trendChart?.dispose()
  storeChart?.dispose()
})

function goto(name: string) {
  router.push({ name: 'repo-detail', params: { name } })
}
</script>

<style scoped>
.stat {
  text-align: center;
  padding: 10px 4px;
}
.stat-label {
  font-size: 12px;
  color: var(--ba-muted);
}
.stat-value {
  font-size: 22px;
  font-weight: 600;
  margin-top: 4px;
}
.stat-sub {
  font-size: 11px;
  margin-top: 2px;
}
</style>
