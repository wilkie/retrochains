int v;
int *ptrs[3] = { &v, 0 };
int *get(int i) {
  return ptrs[i];
}
