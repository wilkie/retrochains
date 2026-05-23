int add1(int x);

int (*fp)(int) = add1;

int via(int v) {
  return fp(v);
}
