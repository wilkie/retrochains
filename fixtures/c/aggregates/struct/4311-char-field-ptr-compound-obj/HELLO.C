struct S { int i; char c; };
int rd(struct S *p) { p->c += 5; return p->c; }
void wr(struct S *p) { p->c -= 2; }
int main(void) { return 0; }
