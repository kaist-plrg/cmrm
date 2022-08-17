/* Generated by CIL v. 1.8.2 */
/* print_CIL_Input is true */

struct __pthread_internal_list {
   struct __pthread_internal_list *__prev ;
   struct __pthread_internal_list *__next ;
};
typedef struct __pthread_internal_list __pthread_list_t;
struct __pthread_mutex_s {
   int __lock ;
   unsigned int __count ;
   int __owner ;
   unsigned int __nusers ;
   int __kind ;
   short __spins ;
   short __elision ;
   __pthread_list_t __list ;
};
union __anonunion_pthread_mutex_t_335460617 {
   struct __pthread_mutex_s __data ;
   char __size[40] ;
   long __align ;
};
typedef union __anonunion_pthread_mutex_t_335460617 pthread_mutex_t;
#pragma merger("0","/tmp/cil-EDju2Ez5.i","-pthread")
extern  __attribute__((__nothrow__)) int ( __attribute__((__nonnull__(1))) pthread_mutex_lock)(pthread_mutex_t *__mutex ) ;
extern  __attribute__((__nothrow__)) int ( __attribute__((__nonnull__(1))) pthread_mutex_unlock)(pthread_mutex_t *__mutex ) ;
int n  =    0;
pthread_mutex_t m  =    {{0, 0U, 0, 0U, 0, (short)0, (short)0, {(struct __pthread_internal_list *)0, (struct __pthread_internal_list *)0}}};
void f1(int i ) 
{ 


  {
  if (i <= 0) {
    pthread_mutex_lock(& m);
  } else {
    f1(i - 1);
    n *= i;
  }
  return;
}
}
void f2(int i ) 
{ 


  {
  if (i <= 0) {
    pthread_mutex_unlock(& m);
  } else {
    n *= i;
    f1(i - 1);
  }
  return;
}
}
void f3(void) 
{ 


  {
  f1(5);
  f2(5);
  return;
}
}
int main(void) 
{ 


  {
  return (0);
}
}
#pragma merger("0","/tmp/cil-518cZWw0.i","-pthread")
