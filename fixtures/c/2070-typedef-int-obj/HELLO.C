typedef int mytype;
typedef mytype *mytype_ptr;
int main(void) {
  mytype x = 10;
  mytype_ptr p = &x;
  return *p + x;
}
