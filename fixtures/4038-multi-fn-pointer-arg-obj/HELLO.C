void incr(int *p) {
  *p = *p + 1;
}
int main(void) {
  int x = 41;
  incr(&x);
  return x;
}
