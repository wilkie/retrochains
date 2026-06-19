struct S { int i; char c; };
int rd(struct S *p) { p->c++; return p->c; }
void wr(struct S *p) { p->c--; }
int main(void) { return 0; }
