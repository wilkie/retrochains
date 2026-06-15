struct N { int v; };
struct N a;
struct N b;
struct N *arr[2] = { &a, &b };
int peek(int i) {
  return arr[i]->v;
}
