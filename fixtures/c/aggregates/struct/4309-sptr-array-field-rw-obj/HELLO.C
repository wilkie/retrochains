struct S { int a[4]; int b[4]; char c[4]; };
int rd_a(struct S *s, int i) { return s->a[i]; }
int rd_b(struct S *s, int i) { return s->b[i]; }
int rd_c(struct S *s, int i) { return s->c[i]; }
void wr_a(struct S *s, int i, int v) { s->a[i] = v; }
void wr_c(struct S *s, int i, int v) { s->c[i] = v; }
int main(void) { return 0; }
