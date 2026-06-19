struct S { int a[4]; };
int sum2(struct S *s, int i, int j) { return s->a[i] + s->a[j]; }
int main(void) { return 0; }
