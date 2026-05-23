struct S { int v; };

struct S *arr[3];

int pick(int i) {
  return arr[i]->v;
}
