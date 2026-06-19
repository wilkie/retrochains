struct S { int i; char c; };
void o(struct S *p) { p->c |= 8; }
void a(struct S *p) { p->c &= 15; }
void x(struct S *p) { p->c ^= 4; }
int main(void) { return 0; }
